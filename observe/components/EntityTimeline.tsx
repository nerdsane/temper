import type { EntityEvent } from "@/lib/types";

interface EntityTimelineProps {
  events: EntityEvent[];
}

export default function EntityTimeline({ events }: EntityTimelineProps) {
  if (events.length === 0) {
    return (
      <div className="bg-gray-900 border border-gray-800 rounded-lg p-8 text-center">
        <p className="text-sm text-gray-400">No events recorded yet.</p>
      </div>
    );
  }

  return (
    <div className="relative">
      {/* Timeline line */}
      <div className="absolute left-4 top-2 bottom-2 w-px bg-gray-700" />

      <div className="space-y-0">
        {events.map((event, i) => {
          const isFirst = i === 0;
          const isLast = i === events.length - 1;
          const date = new Date(event.timestamp);
          const timeStr = date.toLocaleTimeString("en-US", {
            hour: "2-digit",
            minute: "2-digit",
            second: "2-digit",
            hour12: false,
          });
          const dateStr = date.toLocaleDateString("en-US", {
            month: "short",
            day: "numeric",
          });

          return (
            <div key={i} className="relative flex gap-4 py-3">
              {/* Timeline dot */}
              <div className="relative z-10 flex-shrink-0">
                <div
                  className={`w-8 h-8 rounded-full flex items-center justify-center border-2 ${
                    isFirst
                      ? "bg-blue-950 border-blue-500"
                      : isLast
                      ? "bg-green-950 border-green-500"
                      : "bg-gray-900 border-gray-600"
                  }`}
                >
                  <div
                    className={`w-2 h-2 rounded-full ${
                      isFirst
                        ? "bg-blue-400"
                        : isLast
                        ? "bg-green-400"
                        : "bg-gray-400"
                    }`}
                  />
                </div>
              </div>

              {/* Event content */}
              <div className="flex-1 bg-gray-900 border border-gray-800 rounded-lg p-3 min-w-0">
                <div className="flex items-center justify-between mb-1">
                  <span className="font-mono text-sm text-blue-400 font-semibold">
                    {event.action}
                  </span>
                  <span className="text-xs text-gray-500 font-mono">
                    {dateStr} {timeStr}
                  </span>
                </div>
                <div className="flex items-center gap-2 text-sm">
                  <span className="font-mono text-gray-400">{event.from_state}</span>
                  <span className="text-gray-600">{"->"}</span>
                  <span className="font-mono text-gray-200">{event.to_state}</span>
                </div>
                <div className="text-xs text-gray-500 mt-1">
                  by {event.actor}
                </div>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
