import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  async rewrites() {
    return [
      {
        source: "/observe/:path*",
        destination: "http://localhost:3000/observe/:path*",
      },
    ];
  },
};

export default nextConfig;
