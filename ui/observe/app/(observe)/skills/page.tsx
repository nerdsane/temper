import { redirect } from "next/navigation";

/** Backward-compatible redirect: /skills -> /os-apps */
export default function SkillsPage() {
  redirect("/os-apps");
}
