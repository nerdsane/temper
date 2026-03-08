"use client";

import dynamic from "next/dynamic";
import LandingNav from "@/components/landing/LandingNav";
import ProgressCard from "@/components/landing/ProgressCard";
import HeroSection from "@/components/landing/HeroSection";
import VisionSection from "@/components/landing/VisionSection";
import NarrativeSection from "@/components/landing/NarrativeSection";
import SpecViz from "@/components/landing/SpecViz";
import VerificationViz from "@/components/landing/VerificationViz";
import MediationViz from "@/components/landing/MediationViz";
import EvolutionViz from "@/components/landing/EvolutionViz";
import SharedStateViz from "@/components/landing/SharedStateViz";
import RoadmapSection from "@/components/landing/RoadmapSection";
import CtaSection from "@/components/landing/CtaSection";
import LandingFooter from "@/components/landing/LandingFooter";
import SectionDivider from "@/components/landing/SectionDivider";

const WebGLBackground = dynamic(() => import("@/components/landing/WebGLBackground"), { ssr: false });

export default function LandingPage() {
  return (
    <div className="scroll-smooth">
      <WebGLBackground />

      {/* Ambient orbs */}
      <div className="fixed rounded-full pointer-events-none z-0 w-[600px] h-[600px] -top-[10%] -left-[5%] bg-[radial-gradient(circle,rgba(45,212,191,0.04)_0%,transparent_70%)] animate-orb-drift-1 max-sm:hidden" />
      <div className="fixed rounded-full pointer-events-none z-0 w-[500px] h-[500px] -bottom-[10%] -right-[8%] bg-[radial-gradient(circle,rgba(139,92,246,0.03)_0%,transparent_70%)] animate-orb-drift-2 max-sm:hidden" />

      {/* Ambient mesh gradient overlay */}
      <div className="fixed inset-0 z-0 pointer-events-none bg-[radial-gradient(ellipse_80%_60%_at_15%_20%,rgba(45,212,191,0.05)_0%,transparent_60%),radial-gradient(ellipse_60%_50%_at_75%_40%,rgba(139,92,246,0.03)_0%,transparent_55%),radial-gradient(ellipse_70%_60%_at_50%_80%,rgba(45,212,191,0.03)_0%,transparent_50%)]" />

      <LandingNav />
      <ProgressCard />

      <HeroSection />
      <SectionDivider />

      <VisionSection />
      <SectionDivider />

      <NarrativeSection
        id="how"
        step="01"
        label="The Spec"
        title="Declarative Intent."
        paragraphs={[
          "Everything in Temper starts as a specification. Agents describe what they need — state machines, data models, and policies — and the kernel derives the runtime behavior.",
          "If a transition is not in the spec, it cannot happen. If a policy is not in the store, it is denied. The kernel interprets intent directly.",
        ]}
      >
        <SpecViz />
      </NarrativeSection>
      <SectionDivider />

      <NarrativeSection
        id="step-proof"
        step="02"
        label="The Proof"
        title="Verification Cascade."
        paragraphs={[
          "Before a spec is deployed, it must survive four levels of mathematical proof. We prove correctness across every possible execution trace.",
          "The model checker verifies the actual Rust code that runs in production. If you can't prove it, you can't deploy it.",
        ]}
        reversed
      >
        <VerificationViz />
      </NarrativeSection>
      <SectionDivider />

      <NarrativeSection
        id="step-mediation"
        step="03"
        label="Mediation"
        title="Cedar Authorization."
        paragraphs={[
          "Every action flows through a Cedar authorization engine. Temper operates on a default-deny posture. Access is never assumed.",
          "Denied actions surface as pending decisions. Approve once, and Temper generates the policy that governs the agent forever.",
        ]}
      >
        <MediationViz />
      </NarrativeSection>
      <SectionDivider />

      <NarrativeSection
        id="step-evolution"
        step="04"
        label="Evolution"
        title="Closing the Loop."
        paragraphs={[
          "Failures in Temper aren't errors — they are training data. Every denied action is recorded as a trajectory entry.",
          "The Evolution Engine analyzes these patterns and proposes spec updates. You approve the verified fix, and the agent OS grows.",
        ]}
        reversed
      >
        <EvolutionViz />
      </NarrativeSection>
      <SectionDivider />

      <NarrativeSection
        id="step-multiagent"
        label="Multi-Agent"
        title="Verified Shared State."
        paragraphs={[
          "Agents coordinate through a shared state layer queryable via OData. Every entity is a verified state machine, ensuring coordination remains safe.",
          "Background executors claim tasks, agents spawn children with scoped permissions, and everyone operates on the same event-sourced truth.",
        ]}
      >
        <SharedStateViz />
      </NarrativeSection>
      <SectionDivider />

      <RoadmapSection />
      <SectionDivider />

      <CtaSection />
      <SectionDivider />

      <LandingFooter />
    </div>
  );
}
