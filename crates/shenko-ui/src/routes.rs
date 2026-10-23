use axum::response::{Html, Json};
use serde_json::json;

pub async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "product": "Shenko", "version": "0.1.0"}))
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Shenko（深刻）</title>
    <style>
      :root {
        --bg: #0f0f0f;
        --surface: #1a1a1a;
        --border: #2a2a2a;
        --text: #e8e8e8;
        --muted: #888;
        --accent: oklch(70% 0.18 250);
        --accent-dim: oklch(50% 0.14 250);
      }
      * { box-sizing: border-box; margin: 0; padding: 0; }
      body {
        background: var(--bg);
        color: var(--text);
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
        font-size: 15px;
        line-height: 1.6;
        height: 100dvh;
        display: flex;
        flex-direction: column;
      }
      header {
        padding: 12px 20px;
        border-bottom: 1px solid var(--border);
        display: flex;
        align-items: center;
        gap: 12px;
      }
      header h1 {
        font-size: 17px;
        font-weight: 600;
        letter-spacing: -0.01em;
      }
      .status-dot {
        width: 8px; height: 8px;
        border-radius: 50%;
        background: var(--accent-dim);
      }
      .status-dot.active { background: var(--accent); }
      main {
        flex: 1;
        overflow-y: auto;
        padding: 20px;
        display: flex;
        flex-direction: column;
        gap: 12px;
      }
      .message {
        max-width: 680px;
        padding: 12px 16px;
        border-radius: 12px;
        line-height: 1.65;
      }
      .message.subject {
        background: var(--surface);
        border: 1px solid var(--border);
      }
      .message.user {
        background: var(--accent-dim);
        margin-left: auto;
      }
      footer {
        padding: 12px 20px;
        border-top: 1px solid var(--border);
        display: flex;
        gap: 8px;
      }
      textarea {
        flex: 1;
        background: var(--surface);
        border: 1px solid var(--border);
        border-radius: 8px;
        color: var(--text);
        padding: 10px 14px;
        font-size: 14px;
        resize: none;
        height: 44px;
        min-height: 44px;
        max-height: 200px;
      }
      textarea:focus { outline: none; border-color: var(--accent); }
      button {
        background: var(--accent);
        border: none;
        border-radius: 8px;
        color: #000;
        padding: 0 18px;
        font-size: 14px;
        font-weight: 600;
        cursor: pointer;
        height: 44px;
      }
      button:hover { opacity: 0.9; }
    </style>
  </head>
  <body>
    <header>
      <div class="status-dot" id="dot"></div>
      <h1>Shenko（深刻）</h1>
    </header>
    <main id="timeline"></main>
    <footer>
      <textarea id="input" placeholder="发消息…" rows="1"></textarea>
      <button id="send">发送</button>
    </footer>
    <script>
      const timeline = document.getElementById('timeline');
      const input = document.getElementById('input');
      const dot = document.getElementById('dot');
      let ws;

      function connect() {
        const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        ws = new WebSocket(proto + '//' + location.host + '/ws');
        ws.onopen = () => { dot.classList.add('active'); };
        ws.onclose = () => {
          dot.classList.remove('active');
          setTimeout(connect, 3000);
        };
        ws.onmessage = (ev) => {
          const msg = JSON.parse(ev.data);
          appendMessage(msg.role || 'subject', msg.content || '');
        };
      }

      function appendMessage(role, content) {
        const div = document.createElement('div');
        div.className = 'message ' + role;
        div.textContent = content;
        timeline.appendChild(div);
        timeline.scrollTop = timeline.scrollHeight;
      }

      document.getElementById('send').addEventListener('click', () => {
        const text = input.value.trim();
        if (!text) return;
        appendMessage('user', text);
        if (ws && ws.readyState === 1) {
          ws.send(JSON.stringify({ role: 'user', content: text }));
        }
        input.value = '';
      });

      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' && !e.shiftKey) {
          e.preventDefault();
          document.getElementById('send').click();
        }
      });

      connect();
    </script>
  </body>
  </html>"#;
