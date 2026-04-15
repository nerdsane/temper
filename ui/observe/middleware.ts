import { auth } from "@/auth";
import { NextResponse } from "next/server";

export default auth((req) => {
  // Allow auth routes through without authentication.
  if (req.nextUrl.pathname.startsWith("/api/auth")) {
    return NextResponse.next();
  }

  const useGitHubAuth = !!process.env.AUTH_GITHUB_ID;

  // If GitHub OAuth is enabled, require authentication.
  if (useGitHubAuth && !req.auth) {
    const signInUrl = new URL("/api/auth/signin", req.url);
    signInUrl.searchParams.set("callbackUrl", req.url);
    return NextResponse.redirect(signInUrl);
  }

  const requestHeaders = new Headers(req.headers);

  if (useGitHubAuth) {
    // GitHub OAuth mode: resolve identity and role from the session.
    const username = (req.auth?.githubUsername || req.auth?.user?.name || "unknown").trim() || "unknown";
    const adminAllowlist = (process.env.TEMPER_ADMIN_GITHUB_USERS || "")
      .split(",")
      .map((v) => v.trim().toLowerCase())
      .filter((v) => v.length > 0);
    const principalKind = adminAllowlist.includes(username.toLowerCase()) ? "admin" : "customer";
    requestHeaders.set("X-Temper-Principal-Id", `github:${username}`);
    requestHeaders.set("X-Temper-Principal-Kind", principalKind);
  } else {
    // API-key-only mode: the Observe UI is the admin — no user identity needed.
    requestHeaders.set("X-Temper-Principal-Id", "observe-ui");
    requestHeaders.set("X-Temper-Principal-Kind", "admin");
  }

  // Inject Bearer token for API authentication when configured.
  const apiKey = process.env.TEMPER_API_KEY;
  if (apiKey) {
    requestHeaders.set("Authorization", `Bearer ${apiKey}`);
  }

  return NextResponse.next({
    request: { headers: requestHeaders },
  });
});

export const config = {
  matcher: [
    "/dashboard/:path*",
    "/activity/:path*",
    "/agents/:path*",
    "/decisions/:path*",
    "/entities/:path*",
    "/evolution/:path*",
    "/feature-requests/:path*",
    "/integrations/:path*",
    "/os-apps/:path*",
    "/skills/:path*",
    "/specs/:path*",
    "/verify/:path*",
    "/workflows/:path*",
    // API proxy paths — middleware injects Authorization header before Next.js rewrites proxy to Temper API.
    "/observe/:path*",
    "/api/:path*",
  ],
};
