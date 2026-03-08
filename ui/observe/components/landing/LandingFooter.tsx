export default function LandingFooter() {
  return (
    <footer className="py-10 pb-6 relative z-[2]">
      <div className="max-w-[960px] mx-auto px-6">
        <div className="flex items-center justify-between flex-wrap gap-4 max-sm:flex-col max-sm:items-start">
          <div className="flex items-center gap-2 text-[13px] text-zinc-600">
            Temper &middot; MIT / Apache-2.0
          </div>
          <ul className="flex gap-5 list-none m-0 p-0">
            <li>
              <a
                href="https://github.com/nerdsane/temper"
                target="_blank"
                rel="noopener"
                className="text-[13px] text-zinc-600 hover:text-zinc-400 no-underline transition-colors"
              >
                GitHub
              </a>
            </li>
            <li>
              <a
                href="https://github.com/nerdsane/temper/blob/main/docs/PAPER.md"
                target="_blank"
                rel="noopener"
                className="text-[13px] text-zinc-600 hover:text-zinc-400 no-underline transition-colors"
              >
                Paper
              </a>
            </li>
          </ul>
        </div>
        <div className="mt-5 pt-4 border-t border-white/[0.04] text-xs text-zinc-600 leading-[1.6]">
          Built by{" "}
          <a href="https://github.com/nerdsane" target="_blank" rel="noopener" className="text-zinc-400 hover:text-teal-400 no-underline transition-colors">
            Sesh Nalla
          </a>{" "}
          &amp;{" "}
          <a href="https://github.com/rita-aga" target="_blank" rel="noopener" className="text-zinc-400 hover:text-teal-400 no-underline transition-colors">
            Rita Agafonova
          </a>
        </div>
      </div>
    </footer>
  );
}
