"use client";

import { useRef, useEffect, useState, type ReactNode } from "react";

type Animation = "fade-up" | "zoom" | "slide-left" | "slide-right";

const hiddenClasses: Record<Animation, string> = {
  "fade-up": "translate-y-4 opacity-0",
  "zoom": "translate-y-4 scale-[0.97] opacity-0",
  "slide-left": "-translate-x-6 opacity-0",
  "slide-right": "translate-x-6 opacity-0",
};

export default function ScrollReveal({
  children,
  animation = "fade-up",
  delay = 0,
  className = "",
}: {
  children: ReactNode;
  animation?: Animation;
  delay?: number;
  className?: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          setVisible(true);
          observer.unobserve(el);
        }
      },
      { threshold: 0.1 },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  return (
    <div
      ref={ref}
      className={`transition-all duration-600 ${visible ? "translate-y-0 translate-x-0 scale-100 opacity-100" : hiddenClasses[animation]} ${className}`}
      style={{ transitionDelay: `${delay}ms`, transitionTimingFunction: 'cubic-bezier(0.16, 1, 0.3, 1)' }}
    >
      {children}
    </div>
  );
}
