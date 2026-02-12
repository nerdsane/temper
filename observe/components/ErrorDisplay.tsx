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
        <div className="inline-flex items-center justify-center w-12 h-12 rounded-full bg-red-950/50 border border-red-900 mb-4">
          <svg className="w-6 h-6 text-red-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeWidth={1.5}
              d="M12 9v3.75m9-.75a9 9 0 11-18 0 9 9 0 0118 0zm-9 3.75h.008v.008H12v-.008z"
            />
          </svg>
        </div>
        <h3 className="text-lg font-semibold text-gray-200 mb-1">{title}</h3>
        <p className="text-sm text-gray-400 mb-4">{message}</p>
        <div className="flex items-center justify-center gap-3">
          {retry && (
            <button
              onClick={retry}
              className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm rounded-md transition-colors"
            >
              Retry
            </button>
          )}
          <Link
            href={backHref}
            className="px-4 py-2 bg-gray-800 hover:bg-gray-700 text-gray-300 text-sm rounded-md transition-colors border border-gray-700"
          >
            {backLabel}
          </Link>
        </div>
      </div>
    </div>
  );
}
