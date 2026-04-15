import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import ErrorBoundary from "@/components/ErrorBoundary";

// Suppress React error boundary console.error during tests
beforeEach(() => {
  vi.spyOn(console, "error").mockImplementation(() => {});
});

// A child that can be controlled from outside via a ref callback
let throwControl = true;
function ControllableChild() {
  if (throwControl) throw new Error("Test crash");
  return <div>Child content</div>;
}

describe("ErrorBoundary", () => {
  it("renders children when no error", () => {
    throwControl = false;
    render(
      <ErrorBoundary>
        <ControllableChild />
      </ErrorBoundary>,
    );
    expect(screen.getByText("Child content")).toBeInTheDocument();
  });

  it("shows error UI when child throws", () => {
    throwControl = true;
    render(
      <ErrorBoundary>
        <ControllableChild />
      </ErrorBoundary>,
    );
    expect(screen.getByText("Something went wrong")).toBeInTheDocument();
    expect(screen.getByText("Test crash")).toBeInTheDocument();
  });

  it("shows try again button that resets error state", () => {
    throwControl = true;
    render(
      <ErrorBoundary>
        <ControllableChild />
      </ErrorBoundary>,
    );

    expect(screen.getByText("Something went wrong")).toBeInTheDocument();
    expect(screen.getByText("Try Again")).toBeInTheDocument();

    // Stop throwing before clicking retry
    throwControl = false;
    fireEvent.click(screen.getByText("Try Again"));

    expect(screen.getByText("Child content")).toBeInTheDocument();
  });

  it("renders custom fallback when provided", () => {
    throwControl = true;
    render(
      <ErrorBoundary fallback={<div>Custom fallback</div>}>
        <ControllableChild />
      </ErrorBoundary>,
    );
    expect(screen.getByText("Custom fallback")).toBeInTheDocument();
  });

  it("logs error to console", () => {
    throwControl = true;
    render(
      <ErrorBoundary>
        <ControllableChild />
      </ErrorBoundary>,
    );
    expect(console.error).toHaveBeenCalled();
  });
});
