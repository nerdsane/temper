"use client";

import Image from "next/image";
import { useSession, signOut } from "next-auth/react";

export default function UserMenu() {
  const { data: session } = useSession();

  if (!session?.user) return null;

  return (
    <div className="flex items-center gap-2 px-3 py-2 text-xs text-zinc-400">
      {session.user.image && (
        <Image
          src={session.user.image}
          alt=""
          width={20}
          height={20}
          className="rounded-full"
        />
      )}
      <span className="truncate max-w-[100px]">{session.user.name}</span>
      <button
        onClick={() => signOut()}
        className="ml-auto text-zinc-500 hover:text-zinc-300 transition-colors"
      >
        Sign out
      </button>
    </div>
  );
}
