"use client";

import dynamic from "next/dynamic";
import LandingNav from "@/components/landing/LandingNav";
import ProgressCard from "@/components/landing/ProgressCard";
import HeroSection from "@/components/landing/HeroSection";
import VisionSection from "@/components/landing/VisionSection";
import NarrativeSection from "@/components/landing/NarrativeSection";
import TemperedAgentViz from "@/components/landing/TemperedAgentViz";
import KernelViz from "@/components/landing/KernelViz";
import AppsViz from "@/components/landing/AppsViz";
import VerificationViz from "@/components/landing/VerificationViz";
import MediationViz from "@/components/landing/MediationViz";
import RecordedViz from "@/components/landing/RecordedViz";
import GrowthSection from "@/components/landing/GrowthSection";
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

      {/* 1. Hero */}
      <HeroSection />
      <SectionDivider />

      {/* 2. What's a Tempered Agent? */}
      <VisionSection />
      <SectionDivider />

      {/* 3. The Kernel */}
      <NarrativeSection
        id="step-kernel"
        step="01"
        label="The Kernel"
        title="The Foundation."
        paragraphs={[
          "Temper starts with a kernel. A spec interpreter that reads what agents describe, a verification engine that proves it correct, Cedar authorization that governs every action, and event-sourced persistence that remembers everything.",
          "The kernel is the invariant. Everything else — every app, every capability, every integration — is built on top of it.",
        ]}
      >
        <KernelViz />
      </NarrativeSection>
      <SectionDivider />

      {/* 4. Apps, Not Code */}
      <NarrativeSection
        id="step-apps"
        step="02"
        label="Apps"
        title="Apps, Not Code."
        paragraphs={[
          "Agents don't write code. They describe what they need — state machines, data models, policies — and the kernel derives the runtime: API endpoints, persistence, authorization rules, observability. All from a single spec.",
          "Nobody writes specs by hand. Agents generate them. But here's what one looks like — a state machine with transitions, guards, and effects, expressed as declarative intent.",
        ]}
        reversed
      >
        <AppsViz />
      </NarrativeSection>
      <SectionDivider />

      {/* 5. The Proof */}
      <NarrativeSection
        id="step-proof"
        step="03"
        label="The Proof"
        title="Verification Cascade."
        paragraphs={[
          "Before any spec deploys, it survives four levels of mathematical proof — syntax validation, SMT constraint solving, model checking across every reachable state, and property-based testing.",
          "The model checker verifies the actual Rust code that runs in production. If you can't prove it, you can't deploy it.",
        ]}
      >
        <VerificationViz />
      </NarrativeSection>
      <SectionDivider />

      {/* 6. Every Action Has a Policy */}
      <NarrativeSection
        id="step-policy"
        step="04"
        label="Governance"
        title="Every Action Has a Policy."
        paragraphs={[
          "Temper operates on a default-deny posture. Every action flows through a Cedar authorization engine. Access is never assumed — it is always derived from policy.",
          "Denied actions aren't errors. They surface as pending decisions. Approve once, and Temper generates the Cedar policy that governs the agent forever.",
        ]}
        reversed
      >
        <MediationViz />
      </NarrativeSection>
      <SectionDivider />

      {/* 7. Everything Is Recorded */}
      <NarrativeSection
        id="step-recorded"
        step="05"
        label="Trajectories"
        title="Everything Is Recorded."
        paragraphs={[
          "Every state transition carries the agent's identity, the action taken, the before and after state, and whether authorization succeeded or was denied. Every question has an answer in the trajectory log.",
          "This gives agents three things: an audit trail of everything that happened, self-awareness of where they are and what's blocked, and cross-agent visibility into what others have done.",
        ]}
      >
        <RecordedViz />
      </NarrativeSection>
      <SectionDivider />

      {/* 8. Watch an Agent Grow (special section) */}
      <GrowthSection />
      <SectionDivider />

      {/* 9. Closing the Loop */}
      <NarrativeSection
        id="step-evolution"
        step="06"
        label="Evolution"
        title="Closing the Loop."
        paragraphs={[
          "The evolution engine analyzes trajectory patterns — repeated failures, friction points, unmet intents — and surfaces spec proposals through an O-P-A-D-I record chain: Observation, Problem, Analysis, Decision, Impact.",
          "The agent proposes changes to its own harness. You hold the gate. Each cycle adds a new capability to the agent's operating environment.",
        ]}
        reversed
      >
        <EvolutionViz />
      </NarrativeSection>
      <SectionDivider />

      {/* 10. Shared State, Not Messages */}
      <NarrativeSection
        id="step-multiagent"
        label="Multi-Agent"
        title="Shared State, Not Messages."
        paragraphs={[
          "Everything a tempered agent builds — task systems, knowledge bases, notification pipelines — is a verified entity queryable through OData. Agents read each other's state, not each other's messages.",
          "One agent's completed step unblocks another's next action. Background executors claim work, children inherit scoped permissions, and everyone operates on the same event-sourced truth.",
        ]}
      >
        <SharedStateViz />
      </NarrativeSection>
      <SectionDivider />

      {/* 11. Roadmap */}
      <RoadmapSection />
      <SectionDivider />

      {/* 12. CTA */}
      <CtaSection />
      <SectionDivider />

      <LandingFooter />
    </div>
  );
}
