import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { AboutDialog } from "./AboutDialog";

describe("AboutDialog", () => {
  it("shows artifact version and commit with accessible close behavior", async () => {
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(
      <AboutDialog
        open
        buildInfo={{
          version: "1.2.3",
          commitHash: "1234567890abcdef1234567890abcdef12345678",
          dirty: false,
        }}
        runtimeInfo={{
          operatingSystem: "macos",
          architecture: "aarch64",
          bundleType: "app",
          updateChannel: "stable",
          updaterConfigured: true,
        }}
        updateState={{ status: "idle" }}
        updateChannel="stable"
        onUpdateChannelChange={vi.fn()}
        onCheckForUpdates={vi.fn()}
        onRestartUpdate={vi.fn()}
        onReportBug={vi.fn()}
        onClose={onClose}
      />,
    );

    expect(
      screen.getByRole("heading", { name: "About Denote" }),
    ).toBeInTheDocument();
    expect(screen.getByText("1.2.3")).toBeInTheDocument();
    expect(screen.getByText("1234567890ab")).toBeInTheDocument();
    expect(
      screen.getByText("1234567890abcdef1234567890abcdef12345678"),
    ).toBeInTheDocument();
    expect(screen.getByText("Clean commit")).toBeInTheDocument();
    expect(screen.getByText("macos · aarch64")).toBeInTheDocument();
    expect(screen.getByText("stable")).toBeInTheDocument();
    expect(
      screen.getByText(
        "Denote checks for and downloads signed stable updates automatically after startup.",
      ),
    ).toBeInTheDocument();
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Close About Denote" }),
      ).toHaveFocus(),
    );

    await user.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("offers report and prepared update restart actions accessibly", async () => {
    const user = userEvent.setup();
    const onRestartUpdate = vi.fn();
    const onReportBug = vi.fn();
    render(
      <AboutDialog
        open
        buildInfo={{
          version: "1.2.3",
          commitHash: "1234567890abcdef1234567890abcdef12345678",
          dirty: false,
        }}
        runtimeInfo={{
          operatingSystem: "windows",
          architecture: "x86_64",
          bundleType: "nsis",
          updateChannel: "stable",
          updaterConfigured: true,
        }}
        updateState={{
          status: "ready",
          update: { version: "1.2.4", notes: "Synthetic release notes." },
        }}
        updateChannel="stable"
        onUpdateChannelChange={vi.fn()}
        onCheckForUpdates={vi.fn()}
        onRestartUpdate={onRestartUpdate}
        onReportBug={onReportBug}
        onClose={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Report a bug" }));
    expect(onReportBug).toHaveBeenCalledOnce();
    await user.click(screen.getByRole("button", { name: "Restart to update" }));
    expect(onRestartUpdate).toHaveBeenCalledWith({
      version: "1.2.4",
      notes: "Synthetic release notes.",
    });
  });

  it("explains the fail-closed unconfigured channel", () => {
    render(
      <AboutDialog
        open
        buildInfo={{
          version: "1.2.3",
          commitHash: "1234567890abcdef1234567890abcdef12345678",
          dirty: false,
        }}
        runtimeInfo={{
          operatingSystem: "linux",
          architecture: "x86_64",
          bundleType: "appimage",
          updateChannel: "stable",
          updaterConfigured: false,
        }}
        updateState={{ status: "idle" }}
        updateChannel="stable"
        onUpdateChannelChange={vi.fn()}
        onCheckForUpdates={vi.fn()}
        onRestartUpdate={vi.fn()}
        onReportBug={vi.fn()}
        onClose={vi.fn()}
      />,
    );

    expect(
      screen.getByText(
        "The stable update channel is not configured with a trusted public key.",
      ),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Check for updates" }),
    ).toBeDisabled();
    expect(
      screen.getByRole("checkbox", { name: /Include beta releases/ }),
    ).toBeDisabled();
  });

  it("opts into beta releases with a labeled checkbox and explains the channel", async () => {
    const user = userEvent.setup();
    const onUpdateChannelChange = vi.fn();
    const props = {
      open: true,
      buildInfo: {
        version: "1.2.3",
        commitHash: "1234567890abcdef1234567890abcdef12345678",
        dirty: false,
      },
      runtimeInfo: {
        operatingSystem: "macos",
        architecture: "aarch64",
        bundleType: "app",
        updateChannel: "stable",
        updaterConfigured: true,
      },
      onUpdateChannelChange,
      onCheckForUpdates: vi.fn(),
      onRestartUpdate: vi.fn(),
      onReportBug: vi.fn(),
      onClose: vi.fn(),
    };
    const { rerender } = render(
      <AboutDialog {...props} updateState={{ status: "current" }} updateChannel="stable" />,
    );
    const beta = screen.getByRole("checkbox", { name: /Include beta releases/ });
    expect(beta).not.toBeChecked();
    expect(screen.getByText("This is the latest stable Denote release.")).toBeInTheDocument();

    await user.click(beta);
    expect(onUpdateChannelChange).toHaveBeenLastCalledWith("beta");

    rerender(
      <AboutDialog
        {...props}
        runtimeInfo={{ ...props.runtimeInfo, updateChannel: "beta" }}
        updateState={{ status: "current" }}
        updateChannel="beta"
      />,
    );
    expect(screen.getByRole("checkbox", { name: /Include beta releases/ })).toBeChecked();
    expect(screen.getByText("beta")).toBeInTheDocument();
    expect(
      screen.getByText("This is the latest Denote release, including betas."),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("checkbox", { name: /Include beta releases/ }));
    expect(onUpdateChannelChange).toHaveBeenLastCalledWith("stable");

    rerender(
      <AboutDialog
        {...props}
        updateState={{
          status: "downloading",
          update: { version: "1.3.0-rc.1", notes: null },
          downloaded: 1,
          total: 2,
        }}
        updateChannel="beta"
      />,
    );
    expect(screen.getByRole("checkbox", { name: /Include beta releases/ })).toBeDisabled();
  });
});
