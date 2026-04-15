import "next-auth";
import "next-auth/jwt";

declare module "next-auth" {
  interface Session {
    githubUsername?: string;
  }
}

declare module "next-auth/jwt" {
  interface JWT {
    githubUsername?: string;
  }
}
