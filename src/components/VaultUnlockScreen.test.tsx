import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { VaultUnlockScreen } from "./VaultUnlockScreen";

describe("VaultUnlockScreen", () => {
  it("can unlock with a one-time recovery code", async () => {
    const user = userEvent.setup();
    const onUnlockWithRecoveryCode = vi.fn().mockResolvedValue(undefined);

    render(
      <VaultUnlockScreen
        vaultName="Notes"
        theme="dark"
        onThemeToggle={vi.fn()}
        onShowVaults={vi.fn()}
        onUnlockWithPassword={vi.fn()}
        onUnlockWithRecoveryCode={onUnlockWithRecoveryCode}
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "Use a recovery code" }),
    );
    await user.type(
      screen.getByRole("textbox", { name: "Recovery code" }),
      "AAAA-BBBB",
    );
    await user.click(screen.getByRole("button", { name: "Unlock vault" }));

    expect(onUnlockWithRecoveryCode).toHaveBeenCalledWith("AAAA-BBBB");
  });

  it("opens sync without unlocking the vault when a sync tool is available", async () => {
    const user = userEvent.setup();
    const onOpenSync = vi.fn();
    const props = {
      vaultName: "Notes", theme: "dark" as const, onThemeToggle: vi.fn(), onShowVaults: vi.fn(),
      onUnlockWithPassword: vi.fn(), onUnlockWithRecoveryCode: vi.fn(),
    };
    const view = render(<VaultUnlockScreen {...props} />);
    expect(screen.queryByRole("button", { name: "Open sync" })).not.toBeInTheDocument();
    view.rerender(<VaultUnlockScreen {...props} onOpenSync={onOpenSync} />);

    await user.click(screen.getByRole("button", { name: "Open sync" }));

    expect(onOpenSync).toHaveBeenCalledTimes(1);
    expect(props.onUnlockWithPassword).not.toHaveBeenCalled();
  });
});
