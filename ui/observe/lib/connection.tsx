"use client";

import { createContext, useContext } from "react";
import { useSSEConnected } from "./sse-context";

interface ConnectionState {
  connected: boolean;
  checking: boolean;
}

const ConnectionContext = createContext<ConnectionState>({ connected: true, checking: true });

export function ConnectionProvider({ children }: { children: React.ReactNode }) {
  const sseConnected = useSSEConnected();

  const state: ConnectionState = {
    connected: sseConnected,
    checking: false,
  };

  return (
    <ConnectionContext.Provider value={state}>{children}</ConnectionContext.Provider>
  );
}

export function useConnection() {
  return useContext(ConnectionContext);
}
