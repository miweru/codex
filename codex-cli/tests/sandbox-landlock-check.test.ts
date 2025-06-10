import { test, expect, vi } from "vitest";

// Mock approvals so handleExecCommand always runs in the sandbox
vi.mock("../src/approvals.js", () => ({
  __esModule: true,
  canAutoApprove: () => ({ type: "auto-approve", runInSandbox: true }) as any,
  isSafeCommand: () => null,
}));

// Silence logger output
vi.mock("../src/utils/logger/log.js", () => ({
  __esModule: true,
  log: () => {},
  isLoggingEnabled: () => false,
}));
// Force getSandbox to think it is running on Linux and simulate a failing Landlock helper.
vi.mock("../src/utils/agent/sandbox/landlock.js", async () => {
  const actual = await vi.importActual<any>("../src/utils/agent/sandbox/landlock.js");
  return {
    __esModule: true,
    ...actual,
    ensureLandlockSupported: vi.fn(() => Promise.reject(new Error(actual.ERROR_WHEN_LANDLOCK_NOT_SUPPORTED))),
  };
});

import { _testGetSandbox, handleExecCommand } from "../src/utils/agent/handle-exec-command.js";
import { ERROR_WHEN_LANDLOCK_NOT_SUPPORTED } from "../src/utils/agent/sandbox/landlock.js";

const originalPlatform = process.platform;

function setPlatform(value: string) {
  Object.defineProperty(process, "platform", { value });
}

// Unit style check for getSandbox()
test("getSandbox throws helpful error when Landlock unsupported", async () => {
  setPlatform("linux");
  await expect(_testGetSandbox(true)).rejects.toThrow(ERROR_WHEN_LANDLOCK_NOT_SUPPORTED);
  setPlatform(originalPlatform);
});

// Integration path through handleExecCommand

test("handleExecCommand surfaces Landlock failure", async () => {
  setPlatform("linux");
  const execInput = { cmd: ["echo", "hi"], workdir: undefined, timeoutInMillis: 1000 } as any;
  const config = { model: "any", instructions: "" } as any;
  const policy = { mode: "auto" } as any;
  const getConfirmation = async () => ({ review: "yes" }) as any;

  await expect(
    handleExecCommand(execInput, config, policy, [], getConfirmation)
  ).rejects.toThrow(ERROR_WHEN_LANDLOCK_NOT_SUPPORTED);
  setPlatform(originalPlatform);
});
