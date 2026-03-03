"use client";

import { createContext, useContext, useEffect, useState, useCallback } from "react";
import { checkConnection } from "./api";

interface ConnectionState {
  connected: boolean;
  checking: boolean;
}

const ConnectionContext = createContext<ConnectionState>({ connected: true, checking: true });

export function ConnectionProvider({ children }: { children: React.ReactNode }) {
  const [state, setState] = useState<ConnectionState>({ connected: true, checking: true });

  const check = useCallback(async () => {
    const ok = await checkConnection();
    setState({ connected: ok, checking: false });
  }, []);

  useEffect(() => {
    check();
    const id = setInterval(check, 10000);
    return () => clearInterval(id);
  }, [check]);

  return (
    <ConnectionContext.Provider value={state}>{children}</ConnectionContext.Provider>
  );
}

export function useConnection() {
  return useContext(ConnectionContext);
}
