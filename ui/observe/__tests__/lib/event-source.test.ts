import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createVisibleEventSource } from "../../lib/event-source";

const instances: MockSource[] = [];
class MockSource {
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  close = vi.fn();
  addEventListener = vi.fn();
  constructor() { instances.push(this); }
}
let hidden = false;
let cleanup: (() => void) | undefined;
function visibility(value: boolean) {
  hidden = value;
  document.dispatchEvent(new Event("visibilitychange"));
}
beforeEach(() => {
  vi.useFakeTimers();
  instances.length = 0;
  hidden = false;
  vi.spyOn(document, "hidden", "get").mockImplementation(() => hidden);
  vi.stubGlobal("EventSource", MockSource);
});
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});
it("closes hidden streams and cancels retries until the tab becomes visible", () => {
  cleanup = createVisibleEventSource("/events", "event", vi.fn());
  instances[0].onerror?.();
  visibility(true);
  vi.advanceTimersByTime(60000);
  expect(instances).toHaveLength(1);
  visibility(false);
  expect(instances).toHaveLength(2);
  visibility(true);
  expect(instances[1].close).toHaveBeenCalledOnce();
});
it("does not open a hidden tab or reconnect after cleanup", () => {
  hidden = true;
  cleanup = createVisibleEventSource("/events", "event", vi.fn());
  expect(instances).toHaveLength(0);
  visibility(false);
  instances[0].onerror?.();
  cleanup();
  cleanup = undefined;
  visibility(true);
  visibility(false);
  vi.advanceTimersByTime(60000);
  expect(instances).toHaveLength(1);
});
