import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { CodexRemoteStatusRow } from "@/components/proxy/CodexRemoteStatusRow";
import type { CodexRemoteStatus } from "@/types/proxy";

const mocks = vi.hoisted(() => ({
  query: {
    data: undefined as CodexRemoteStatus | undefined,
    error: null as Error | null,
  },
  mutation: {
    mutate: vi.fn(),
    isPending: false,
    error: null as Error | null,
  },
}));

vi.mock("@/lib/query/proxy", () => ({
  useCodexRemoteStatus: () => mocks.query,
  useCheckCodexRemote: () => mocks.mutation,
}));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    i18n: { language: "en" },
  }),
}));

function remote(state: CodexRemoteStatus["state"]): CodexRemoteStatus {
  return {
    state,
    lastCheckedAt: "2026-09-06T05:00:00Z",
    lastConnectedAt: null,
    lastError: null,
    nextCheckAt: "2026-09-06T05:00:30Z",
    cliPath: null,
  };
}

describe("CodexRemoteStatusRow", () => {
  beforeEach(() => {
    mocks.query.data = remote("connected");
    mocks.query.error = null;
    mocks.mutation.error = null;
    mocks.mutation.isPending = false;
    mocks.mutation.mutate.mockReset();
  });

  it("does not present Desktop ownership as a verified connection", () => {
    mocks.query.data = remote("desktop");
    render(<CodexRemoteStatusRow />);
    expect(screen.getByRole("status")).toHaveTextContent(
      "codexRemote.states.desktop",
    );
    expect(
      screen.queryByText("codexRemote.states.connected"),
    ).not.toBeInTheDocument();
  });

  it("shows the remote failure and next check", () => {
    mocks.query.data = {
      ...remote("degraded"),
      lastError: "Control socket unavailable",
    };
    render(<CodexRemoteStatusRow />);
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Control socket unavailable",
    );
    expect(screen.getByText("codexRemote.nextCheck")).toBeInTheDocument();
  });

  it("requests a new check without toggling takeover", () => {
    render(<CodexRemoteStatusRow />);
    fireEvent.click(screen.getByRole("button", { name: "codexRemote.check" }));
    expect(mocks.mutation.mutate).toHaveBeenCalledOnce();
  });

  it("disables repeat checks while one is being requested", () => {
    mocks.mutation.isPending = true;
    render(<CodexRemoteStatusRow />);
    expect(
      screen.getByRole("button", { name: "codexRemote.check" }),
    ).toBeDisabled();
  });

  it("shows query errors instead of stale connected status", () => {
    mocks.query.error = new Error("Status unavailable");
    render(<CodexRemoteStatusRow />);
    expect(screen.getByRole("status")).toHaveTextContent(
      "codexRemote.states.degraded",
    );
    expect(screen.getByRole("alert")).toHaveTextContent("Status unavailable");
  });
});
