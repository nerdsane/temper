import NextAuth from "next-auth";
import GitHub from "next-auth/providers/github";

export const { handlers, signIn, signOut, auth } = NextAuth({
  providers: [GitHub],
  callbacks: {
    jwt({ token, profile }) {
      if (profile?.login) {
        token.githubUsername = profile.login as string;
      }
      return token;
    },
    session({ session, token }) {
      if (typeof token.githubUsername === "string") {
        session.githubUsername = token.githubUsername;
      }
      return session;
    },
  },
});
