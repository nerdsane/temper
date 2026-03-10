import { auth } from "@/auth";
import { NextResponse } from "next/server";

export default auth((req) => {
  // Allow auth routes through without authentication.
  if (req.nextUrl.pathname.startsWith("/api/auth")) {
    return NextResponse.next();
  }

  // Dev mode bypass: skip OAuth when no GitHub provider is configured.
  const devBypass = !process.env.AUTH_GITHUB_ID;

  // If not authenticated and not in dev bypass, redirect to sign-in.
  if (!req.auth && !devBypass) {
    const signInUrl = new URL("/api/auth/signin", req.url);
    signInUrl.searchParams.set("callbackUrl", req.url);
    return NextResponse.redirect(signInUrl);
  }

  // For authenticated requests proxied to the Temper API, inject identity headers.
  const username = devBypass
    ? (process.env.TEMPER_ADMIN_GITHUB_USERS || "dev-user").split(",")[0].trim()
    : (req.auth?.githubUsername || req.auth?.user?.name || "unknown").trim() || "unknown";
  const adminAllowlist = (process.env.TEMPER_ADMIN_GITHUB_USERS || "")
    .split(",")
    .map((value) => value.trim().toLowerCase())
    .filter((value) => value.length > 0);
  const principalKind = adminAllowlist.includes(username.toLowerCase()) ? "admin" : "customer";

  const requestHeaders = new Headers(req.headers);
  requestHeaders.set("X-Temper-Principal-Id", `github:${username}`);
  requestHeaders.set("X-Temper-Principal-Kind", principalKind);

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
    "/specs/:path*",
    "/verify/:path*",
    "/workflows/:path*",
    // API proxy paths — middleware injects Authorization header before Next.js rewrites proxy to Temper API.
    "/observe/:path*",
    "/api/:path*",
  ],
};
