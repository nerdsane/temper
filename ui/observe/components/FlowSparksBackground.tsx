"use client";
import { useEffect, useRef } from "react";

export default function FlowSparksBackground() {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    // Dark Temper palette RGB values
    const c1 = [147, 51, 234];  // violet
    const c2 = [217, 119, 6];   // bronze/orange
    const c3 = [59, 130, 246];  // blue

    let particles: Array<{
      x: number;
      y: number;
      vx: number;
      vy: number;
      size: number;
      life: number;
      decay: number;
      flicker: number;
    }> = [];
    let t = 0;
    let animId: number;

    function resize() {
      canvas!.width = window.innerWidth;
      canvas!.height = window.innerHeight;
    }

    function initParticles() {
      particles = [];
      for (let i = 0; i < 80; i++) {
        particles.push({
          x: Math.random() * canvas!.width,
          y: canvas!.height * 0.5 + Math.random() * canvas!.height * 0.5,
          vx: (Math.random() - 0.5) * 0.4,
          vy: -(Math.random() * 0.6 + 0.3),
          size: Math.random() * 2.5 + 0.5,
          life: Math.random(),
          decay: 0.0008 + Math.random() * 0.0015,
          flicker: Math.random() * Math.PI * 2,
        });
      }
    }

    function render() {
      const w = canvas!.width,
        h = canvas!.height;
      ctx!.clearRect(0, 0, w, h);

      // Flow blobs
      for (let i = 0; i < 6; i++) {
        const cx =
          w * (0.15 + 0.7 * ((Math.sin(t * 0.2 + i * 1.8) + 1) / 2));
        const cy =
          h * (0.1 + 0.8 * ((Math.cos(t * 0.15 + i * 2.1) + 1) / 2));
        const radius = w * (0.2 + 0.15 * Math.sin(t * 0.3 + i));
        // Bias toward orange/bronze (c2) — 4 of 6 blobs use c2
        const ci = i < 2 ? c2 : i === 2 ? c1 : i === 3 ? c3 : c2;
        const a =
          0.1 * (0.4 + 0.6 * ((Math.sin(t * 0.4 + i * 1.3) + 1) / 2));
        const grd = ctx!.createRadialGradient(cx, cy, 0, cx, cy, radius);
        grd.addColorStop(
          0,
          `rgba(${ci[0]},${ci[1]},${ci[2]},${a})`
        );
        grd.addColorStop(1, "transparent");
        ctx!.fillStyle = grd;
        ctx!.fillRect(0, 0, w, h);
      }

      // Sparks
      const glow = ctx!.createRadialGradient(
        w * 0.5,
        h * 1.1,
        0,
        w * 0.5,
        h * 1.1,
        h * 0.6
      );
      glow.addColorStop(
        0,
        `rgba(${c2[0]},${c2[1]},${c2[2]},${0.07 * 0.6})`
      );
      glow.addColorStop(1, "transparent");
      ctx!.fillStyle = glow;
      ctx!.fillRect(0, 0, w, h);

      particles.forEach((p) => {
        p.x += p.vx + Math.sin(t * 2 + p.flicker) * 0.2;
        p.y += p.vy;
        p.life -= p.decay;
        p.flicker += 0.03;
        if (p.life <= 0 || p.y < -20) {
          p.x = w * 0.2 + Math.random() * w * 0.6;
          p.y = h + Math.random() * 20;
          p.life = 0.6 + Math.random() * 0.4;
          p.vx = (Math.random() - 0.5) * 0.4;
          p.vy = -(Math.random() * 0.6 + 0.3);
        }
        const flicker = 0.4 + Math.sin(p.flicker * 6) * 0.6;
        const a = p.life * 0.8 * 0.6 * flicker;
        const mix = p.life;
        const cr = Math.round(c2[0] * mix + c1[0] * (1 - mix));
        const cg = Math.round(c2[1] * mix + c1[1] * (1 - mix));
        const cb = Math.round(c2[2] * mix + c1[2] * (1 - mix));
        ctx!.beginPath();
        ctx!.arc(p.x, p.y, p.size, 0, Math.PI * 2);
        ctx!.fillStyle = `rgba(${cr},${cg},${cb},${a * 0.6})`;
        ctx!.fill();
        const sparkGrd = ctx!.createRadialGradient(
          p.x,
          p.y,
          0,
          p.x,
          p.y,
          p.size * 8
        );
        sparkGrd.addColorStop(
          0,
          `rgba(${cr},${cg},${cb},${a * 0.15})`
        );
        sparkGrd.addColorStop(1, "transparent");
        ctx!.fillStyle = sparkGrd;
        ctx!.fillRect(
          p.x - p.size * 8,
          p.y - p.size * 8,
          p.size * 16,
          p.size * 16
        );
      });

      t += 0.016;
      animId = requestAnimationFrame(render);
    }

    resize();
    initParticles();
    render();
    window.addEventListener("resize", resize);

    return () => {
      cancelAnimationFrame(animId);
      window.removeEventListener("resize", resize);
    };
  }, []);

  return (
    <canvas
      ref={canvasRef}
      className="fixed inset-0 pointer-events-none"
      style={{ zIndex: 0 }}
    />
  );
}
