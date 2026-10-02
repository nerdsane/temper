/** Keep event streams open only while the document is visible. */
export function createVisibleEventSource(
  url: string,
  eventName: string,
  onMessage: (data: string) => void,
  onConnected: (connected: boolean) => void = () => {},
): () => void {
  let source: EventSource | null = null;
  let retry: ReturnType<typeof setTimeout> | null = null;
  let closed = false;
  let delay = 1000;

  function disconnect() {
    if (retry !== null) clearTimeout(retry);
    retry = null;
    const previous = source;
    source = null;
    previous?.close();
    onConnected(false);
  }

  function connect() {
    if (closed || document.hidden || source) return;
    const current = new EventSource(url);
    source = current;
    current.onopen = () => {
      if (source !== current) return;
      delay = 1000;
      onConnected(true);
    };
    current.addEventListener(eventName, (event) => {
      if (source === current) onMessage((event as MessageEvent).data);
    });
    current.onerror = () => {
      if (source !== current) return;
      disconnect();
      if (!closed && !document.hidden) {
        retry = setTimeout(() => { retry = null; connect(); }, delay);
        delay = Math.min(delay * 2, 30000);
      }
    };
  }

  function visibilityChanged() {
    if (document.hidden) disconnect();
    else connect();
  }
  document.addEventListener("visibilitychange", visibilityChanged);
  connect();
  return () => {
    closed = true;
    document.removeEventListener("visibilitychange", visibilityChanged);
    disconnect();
  };
}
