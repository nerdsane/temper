import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import ErrorDisplay from "@/components/ErrorDisplay";

vi.mock("next/link", () => ({
  default: ({ children, href, ...props }: { children: React.ReactNode; href: string; [key: string]: unknown }) => (
    <a href={href} {...props}>{children}</a>
  ),
}));

describe("ErrorDisplay", () => {
  it("renders the error message", () => {
    render(<ErrorDisplay message="Connection refused" />);
    expect(screen.getByText("Connection refused")).toBeInTheDocument();
  });

  it("renders custom title", () => {
    render(<ErrorDisplay title="Server Error" message="500 Internal Server Error" />);
    expect(screen.getByText("Server Error")).toBeInTheDocument();
  });

  it("renders default title when not provided", () => {
    render(<ErrorDisplay message="Some error" />);
    expect(screen.getByText("Something went wrong")).toBeInTheDocument();
  });

  it("renders retry button when retry callback provided", () => {
    const retry = vi.fn();
    render(<ErrorDisplay message="Error" retry={retry} />);
    const retryButton = screen.getByText("Retry");
    fireEvent.click(retryButton);
    expect(retry).toHaveBeenCalledOnce();
  });

  it("does not render retry button when no callback", () => {
    render(<ErrorDisplay message="Error" />);
    expect(screen.queryByText("Retry")).not.toBeInTheDocument();
  });

  it("renders back link with custom href and label", () => {
    render(<ErrorDisplay message="Error" backHref="/specs" backLabel="Back to Specs" />);
    const link = screen.getByText("Back to Specs");
    expect(link).toHaveAttribute("href", "/specs");
  });
});
