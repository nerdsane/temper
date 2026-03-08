"use client";

import { useRef, useEffect } from "react";
import * as THREE from "three";

export default function WebGLBackground() {
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    let animationId: number;

    try {
      const scene = new THREE.Scene();
      const camera = new THREE.PerspectiveCamera(60, window.innerWidth / window.innerHeight, 0.1, 1000);
      const renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true });
      renderer.setSize(window.innerWidth, window.innerHeight);
      renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));

      // Wireframe cages
      const outerCage = new THREE.Mesh(
        new THREE.IcosahedronGeometry(4, 2),
        new THREE.MeshBasicMaterial({ color: "#2dd4bf", wireframe: true, transparent: true, opacity: 0.08 }),
      );
      const innerCage = new THREE.Mesh(
        new THREE.DodecahedronGeometry(3.5, 1),
        new THREE.MeshBasicMaterial({ color: "#2dd4bf", wireframe: true, transparent: true, opacity: 0.04 }),
      );
      scene.add(outerCage, innerCage);

      // Particles
      const count = 2000;
      const geometry = new THREE.BufferGeometry();
      const pos = new Float32Array(count * 3);
      const aOffset = new Float32Array(count);
      for (let i = 0; i < count; i++) {
        const r = 3.2;
        const theta = Math.random() * Math.PI * 2;
        const phi = Math.acos(2 * Math.random() - 1);
        pos[i * 3] = r * Math.sin(phi) * Math.cos(theta);
        pos[i * 3 + 1] = r * Math.sin(phi) * Math.sin(theta);
        pos[i * 3 + 2] = r * Math.cos(phi);
        aOffset[i] = Math.random() * 100;
      }
      geometry.setAttribute("position", new THREE.BufferAttribute(pos, 3));
      geometry.setAttribute("aOffset", new THREE.BufferAttribute(aOffset, 1));

      const material = new THREE.ShaderMaterial({
        uniforms: {
          uTime: { value: 0 },
          uMouse: { value: new THREE.Vector3(0, 0, 0) },
          uScroll: { value: 0 },
          uColor: { value: new THREE.Color("#2dd4bf") },
          uColorAccent: { value: new THREE.Color("#ffffff") },
        },
        vertexShader: `
          uniform float uTime; uniform vec3 uMouse; uniform float uScroll;
          attribute float aOffset; varying float vGlow; varying float vDist;
          void main() {
            vec3 p = position;
            float t = uTime * 0.4 + aOffset;
            p.x += sin(t * 0.3 + p.y) * 0.5;
            p.y += cos(t * 0.4 + p.z) * 0.5;
            p.z += sin(t * 0.5 + p.x) * 0.5;
            float dToMouse = distance(p.xy, uMouse.xy);
            if (dToMouse < 4.0) { float force = (4.0 - dToMouse) / 4.0; p.xy += (uMouse.xy - p.xy) * force * 0.6; }
            float r = length(p);
            if (r > 3.4) p *= (3.4 / r);
            vec4 mvPosition = modelViewMatrix * vec4(p, 1.0);
            gl_PointSize = (25.0 / -mvPosition.z) * (1.0 + uScroll * 3.0);
            gl_Position = projectionMatrix * mvPosition;
            vGlow = 1.0 - (r / 3.4); vDist = dToMouse;
          }
        `,
        fragmentShader: `
          uniform vec3 uColor; uniform vec3 uColorAccent; varying float vGlow; varying float vDist;
          void main() {
            float d = distance(gl_PointCoord, vec2(0.5));
            if (d > 0.5) discard;
            float strength = pow(1.0 - d * 2.0, 3.0);
            vec3 color = mix(uColor, uColorAccent, vGlow * 1.5);
            gl_FragColor = vec4(color * 2.0, strength * vGlow * 1.2);
          }
        `,
        transparent: true,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      });

      const fire = new THREE.Points(geometry, material);
      scene.add(fire);
      camera.position.z = 10;

      let targetMouseX = 0;
      let targetMouseY = 0;
      let wglScrollY = 0;

      const onMouseMove = (e: MouseEvent) => {
        targetMouseX = (e.clientX / window.innerWidth - 0.5) * 8;
        targetMouseY = -(e.clientY / window.innerHeight - 0.5) * 8;
      };
      const onScroll = () => {
        wglScrollY = window.scrollY;
      };
      const onResize = () => {
        camera.aspect = window.innerWidth / window.innerHeight;
        camera.updateProjectionMatrix();
        renderer.setSize(window.innerWidth, window.innerHeight);
      };

      window.addEventListener("mousemove", onMouseMove);
      window.addEventListener("scroll", onScroll, { passive: true });
      window.addEventListener("resize", onResize);

      function animate(time: number) {
        animationId = requestAnimationFrame(animate);
        const t = time * 0.001;
        const scrollFactor = wglScrollY * 0.0015;
        material.uniforms.uTime.value = t;
        material.uniforms.uScroll.value = Math.min(scrollFactor, 2.0);
        material.uniforms.uMouse.value.x += (targetMouseX - material.uniforms.uMouse.value.x) * 0.05;
        material.uniforms.uMouse.value.y += (targetMouseY - material.uniforms.uMouse.value.y) * 0.05;
        outerCage.rotation.y = t * 0.05 + scrollFactor * 2.5;
        outerCage.rotation.z = t * 0.03 + scrollFactor * 0.5;
        innerCage.rotation.y = -t * 0.08 - scrollFactor * 3.0;
        innerCage.rotation.x = t * 0.04 + scrollFactor * 1.2;
        const lookTarget = new THREE.Vector3(targetMouseX * 0.1, targetMouseY * 0.1, 0);
        camera.position.x += (targetMouseX * 0.2 - camera.position.x) * 0.03;
        camera.position.y += (targetMouseY * 0.2 - camera.position.y) * 0.03;
        camera.lookAt(lookTarget);
        renderer.render(scene, camera);
      }
      animate(0);

      return () => {
        cancelAnimationFrame(animationId);
        window.removeEventListener("mousemove", onMouseMove);
        window.removeEventListener("scroll", onScroll);
        window.removeEventListener("resize", onResize);
        renderer.dispose();
        geometry.dispose();
        material.dispose();
        outerCage.geometry.dispose();
        (outerCage.material as THREE.Material).dispose();
        innerCage.geometry.dispose();
        (innerCage.material as THREE.Material).dispose();
      };
    } catch (e) {
      console.warn("WebGL init failed:", e);
      return () => {
        if (animationId) cancelAnimationFrame(animationId);
      };
    }
  }, []);

  return (
    <canvas
      ref={canvasRef}
      className="fixed top-0 left-0 w-full h-full -z-[1] pointer-events-none opacity-60"
    />
  );
}
