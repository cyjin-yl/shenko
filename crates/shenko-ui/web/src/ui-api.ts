/** HTTP reads and writes share the controller's cancellation lifetime. */
export class UiApi {
  private readonly abort = new AbortController();

  async get(path: string): Promise<unknown> {
    const response = await fetch(path, {
      headers: { Accept: 'application/json' }, cache: 'no-store', signal: this.abort.signal,
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
  }

  async post(path: string, body: unknown): Promise<unknown> {
    const response = await fetch(path, {
      method: 'POST', headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      cache: 'no-store', body: JSON.stringify(body), signal: this.abort.signal,
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
  }

  async workContent(artifactId: string): Promise<{ text: string; revision: number }> {
    const response = await fetch(`/playground/${artifactId}`, { cache: 'no-store', signal: this.abort.signal });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const header = response.headers.get('x-artifact-revision');
    const revision = Number(header);
    if (!header || !Number.isSafeInteger(revision) || revision < 1) throw new Error('invalid work content revision');
    return { text: await response.text(), revision };
  }

  dispose(): void { this.abort.abort(); }
}
