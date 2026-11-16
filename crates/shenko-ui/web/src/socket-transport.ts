import type { ConnectionState } from './projection';

interface SocketEvents {
  connection(state: ConnectionState, seconds: number): void;
  countdown(seconds: number): void;
  opened(): void;
  closed(): void;
  message(event: MessageEvent): void;
}

const RECONNECT_SECONDS = [1, 2, 4, 8, 15];

/** Owns one socket and its reconnect timers; admission facts belong to the controller. */
export class SocketTransport {
  private socket: WebSocket | null = null;
  private disposed = false;
  private attempt = 0;
  private reconnectTimer: number | null = null;
  private countdownTimer: number | null = null;

  constructor(private readonly events: SocketEvents) {}

  get isOpen(): boolean {
    return this.socket?.readyState === WebSocket.OPEN;
  }

  send(text: string): void {
    if (!this.socket || !this.isOpen) throw new Error('WebSocket is not open');
    this.socket.send(text);
  }

  connect(): void {
    if (this.disposed || this.socket) return;
    if (!location.host) { this.events.connection('offline', 0); return; }
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    let socket: WebSocket;
    try { socket = new WebSocket(protocol + '//' + location.host + '/ws'); }
    catch { this.scheduleReconnect(); return; }
    this.socket = socket;
    this.events.connection('connecting', 0);
    socket.onopen = () => {
      if (this.socket !== socket || this.disposed) return;
      this.attempt = 0;
      this.clearTimers();
      this.events.connection('online', 0);
      this.events.opened();
    };
    socket.onmessage = (event) => {
      if (this.socket === socket && !this.disposed) this.events.message(event);
    };
    socket.onerror = () => socket.close();
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      if (this.disposed) return;
      this.events.closed();
      this.scheduleReconnect();
    };
  }

  private scheduleReconnect(): void {
    if (this.disposed) return;
    this.clearTimers();
    const base = RECONNECT_SECONDS[Math.min(this.attempt, RECONNECT_SECONDS.length - 1)];
    this.attempt += 1;
    const delay = Math.max(1, Math.round(base * (0.85 + Math.random() * 0.3)));
    let seconds = delay;
    this.events.connection('reconnecting', seconds);
    this.countdownTimer = window.setInterval(() => {
      seconds = Math.max(0, seconds - 1);
      this.events.countdown(seconds);
    }, 1000);
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay * 1000);
  }

  private clearTimers(): void {
    const clearTimer = typeof window === 'undefined' ? clearTimeout : window.clearTimeout.bind(window);
    const clearCountdown = typeof window === 'undefined' ? clearInterval : window.clearInterval.bind(window);
    if (this.reconnectTimer !== null) clearTimer(this.reconnectTimer);
    if (this.countdownTimer !== null) clearCountdown(this.countdownTimer);
    this.reconnectTimer = null;
    this.countdownTimer = null;
  }

  dispose(): void {
    this.disposed = true;
    this.clearTimers();
    if (!this.socket) return;
    this.socket.onopen = null;
    this.socket.onmessage = null;
    this.socket.onerror = null;
    this.socket.onclose = null;
    this.socket.close();
    this.socket = null;
  }
}
