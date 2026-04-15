import Link from "next/link";

interface ErrorDisplayProps {
  title?: string;
  message: string;
  retry?: () => void;
  backHref?: string;
  backLabel?: string;
}

export default function ErrorDisplay({
  title = "Something went wrong",
  message,
  retry,
  backHref = "/",
  backLabel = "Back to Dashboard",
}: ErrorDisplayProps) {
  return (
    <div className="flex items-center justify-center min-h-[256px]">
      <div className="text-center max-w-md">
        <div className="inline-flex items-center justify-center w-10 h-10 rounded-full bg-[var(--color-accent-pink-dim)] mb-4">
          <svg className="w-5 h-5 text-[var(--color-accent-pink)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeWidth={1.5}
              d="M12 9v3.75m9-.75a9 9 0 11-18 0 9 9 0 0118 0zm-9 3.75h.008v.008H12v-.008z"
            />
          </svg>
        </div>
        <h3 className="text-base font-semibold text-[var(--color-text-primary)] mb-1">{title}</h3>
        <p className="text-sm text-[var(--color-text-secondary)] mb-4">{message}</p>
        <div className="flex items-center justify-center gap-2.5">
          {retry && (
            <button
              onClick={retry}
              className="px-3.5 py-1.5 bg-[var(--color-accent-teal)] hover:bg-[var(--color-accent-teal)] text-[var(--color-bg-primary)] text-sm rounded-[2px] transition-colors focus:outline-none focus:ring-2 focus:ring-[var(--color-accent-teal)] focus:ring-offset-2 focus:ring-offset-[var(--color-bg-primary)]"
            >
              Retry
            </button>
          )}
          <Link
            href={backHref}
            className="px-3.5 py-1.5 bg-[var(--color-bg-elevated)] hover:bg-[var(--color-border-hover)] text-[var(--color-text-secondary)] text-sm rounded-[2px] transition-colors"
          >
            {backLabel}
          </Link>
        </div>
      </div>
    </div>
  );
}
