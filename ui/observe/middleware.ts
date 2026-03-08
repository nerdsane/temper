import { auth } from "@/auth";
import { NextResponse } from "next/server";

export default auth((req) => {
  // Allow auth routes through without authentication.
  if (req.nextUrl.pathname.startsWith("/api/auth")) {
    return NextResponse.next();
  }

  // If not authenticated, redirect to sign-in.
  if (!req.auth) {
    const signInUrl = new URL("/api/auth/signin", req.url);
    signInUrl.searchParams.set("callbackUrl", req.url);
    return NextResponse.redirect(signInUrl);
  }

  // For authenticated requests proxied to the Temper API, inject identity headers.
  const username = (req.auth as any).githubUsername || req.auth.user?.name || "unknown";
  const requestHeaders = new Headers(req.headers);
  requestHeaders.set("X-Temper-Principal-Id", `github:${username}`);
  requestHeaders.set("X-Temper-Principal-Kind", "admin");

  return NextResponse.next({
    request: { headers: requestHeaders },
  });
});

export const config = {
  matcher: [
    // Protect all routes except static files and auth API.
    "/((?!_next/static|_next/image|favicon.ico|api/auth).*)",
  ],
};
