import type { EntityEvent } from "@/lib/types";

interface EntityTimelineProps {
  events: EntityEvent[];
}

export default function EntityTimeline({ events }: EntityTimelineProps) {
  if (events.length === 0) {
    return (
      <div className="bg-[#111115] rounded-lg p-6 text-center">
        <p className="text-[13px] text-zinc-500">No events recorded yet.</p>
      </div>
    );
  }

  return (
    <div className="relative">
      {/* Timeline line */}
      <div className="absolute left-3.5 top-2 bottom-2 w-px bg-white/[0.06]" />

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
            <div key={i} className="relative flex gap-3 py-2">
              {/* Timeline dot */}
              <div className="relative z-10 flex-shrink-0">
                <div
                  className={`w-7 h-7 rounded-full flex items-center justify-center ${
                    isFirst
                      ? "bg-teal-500/10"
                      : isLast
                      ? "bg-lime-500/10"
                      : "bg-zinc-900"
                  }`}
                >
                  <div
                    className={`w-1.5 h-1.5 rounded-full ${
                      isFirst
                        ? "bg-teal-400"
                        : isLast
                        ? "bg-lime-400"
                        : "bg-zinc-500"
                    }`}
                  />
                </div>
              </div>

              {/* Event content */}
              <div className="flex-1 bg-[#111115] rounded-lg p-2.5 min-w-0">
                <div className="flex items-center justify-between mb-0.5">
                  <span className="font-mono text-[13px] text-teal-400 font-medium">
                    {event.action}
                  </span>
                  <span className="text-[10px] text-zinc-600 font-mono">
                    {dateStr} {timeStr}
                  </span>
                </div>
                <div className="flex items-center gap-1.5 text-[13px]">
                  <span className="font-mono text-zinc-500">{event.from_state}</span>
                  <span className="text-zinc-700">{"->"}</span>
                  <span className="font-mono text-zinc-300">{event.to_state}</span>
                </div>
                <div className="text-[11px] text-zinc-600 mt-0.5">
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
