/**
 * Remote commands (fork feature).
 *
 * A teammate that the user explicitly allow-listed can drive this app from the
 * server's control page (https://<server>/remote-control): start/stop the screen
 * share, make the app join/leave a call, restart the app.
 *
 * - Off by default (Settings → Remote access). The local setting is mirrored to
 *   the server, which refuses commands from anybody who is not on the list; this
 *   file asks the Rust side to check the local list again for every command.
 * - Commands arrive over the existing backend websocket ("remote_command") and
 *   are answered with "remote_command_result". Upstream apps ignore the message.
 * - Every accepted command is logged and announced (system notification + toast);
 *   a remotely started share keeps a banner in the main window until it stops.
 */
import { useEffect, useRef } from "react";
import { create } from "zustand";
import toast from "react-hot-toast";
import { isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import useStore, { ParticipantRole } from "@/store/store";
import { socketService } from "@/services/socket";
import { appVersion, tauriUtils } from "@/windows/window-utils";
import { Constants } from "@/constants";
import { sounds } from "@/constants/sounds";
import { endCallAndWait, useEndCall } from "@/lib/hooks";
import { typedInvoke } from "@/core_payloads";
import { components } from "@/openapi";
import { FORK_BUILD_LABEL } from "@/lib/fork-build";

type Room = components["schemas"]["Room"];

type RemoteCommand = "status" | "start_share" | "stop_share" | "join" | "leave" | "restart";

interface RemoteCommandPayload {
  command_id: string;
  command: RemoteCommand;
  from_user_id: string;
  from_name: string;
  room_id?: string;
  issued_at: number;
}

interface CallTokens {
  audioToken: string;
  videoToken: string;
  cameraToken: string;
  participant: string;
  room?: Room | null;
}

/** Commands whose server timestamp is further than this from our clock are refused. */
const MAX_COMMAND_AGE_S = 300;
const SHARE_START_TIMEOUT_MS = 12_000;

type RemoteShareState = {
  /** Name of the teammate who started the current screen share remotely. */
  remoteShareBy: string | null;
  setRemoteShareBy: (name: string | null) => void;
};

export const useRemoteShareStore = create<RemoteShareState>((set) => ({
  remoteShareBy: null,
  setRemoteShareBy: (name) => set({ remoteShareBy: name }),
}));

function statusSnapshot() {
  const { callTokens } = useStore.getState();
  const sharing = callTokens?.role === ParticipantRole.SHARER;
  return {
    in_call: !!callTokens,
    room_id: callTokens?.room?.id ?? "",
    room_name: callTokens?.room?.name ?? "",
    sharing,
    remote_share: sharing && !!useRemoteShareStore.getState().remoteShareBy,
    build: FORK_BUILD_LABEL,
    version: appVersion ?? "",
  };
}

async function api<T>(method: "GET" | "POST" | "PUT", path: string, body?: unknown): Promise<T> {
  const token = useStore.getState().authToken;
  if (!token) throw new Error("Hopp is not signed in on that computer");
  const response = await fetch(`${Constants.backendUrl}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      ...(body !== undefined ? { "Content-Type": "application/json" } : {}),
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (!response.ok) {
    let detail = "";
    try {
      const data = await response.json();
      detail = data?.message || data?.error || "";
    } catch {
      /* no JSON body */
    }
    throw new Error(detail || `Server answered ${response.status}`);
  }
  return (await response.json()) as T;
}

/** Mirror the local setting to the server so it can refuse commands up front. */
async function pushSettingsToServer() {
  if (!useStore.getState().authToken) return;
  const settings = await typedInvoke("get_user_settings");
  await api("PUT", "/api/auth/remote-control/settings", {
    enabled: settings.remote_commands_enabled,
    allowed_user_ids: settings.remote_commands_allowed_users,
  });
}

/** Join a call with microphone and camera off, whatever the user's call settings say. */
async function startCallMuted(tokens: CallTokens, room: Room | null) {
  const { setCallTokens } = useStore.getState();
  sounds.callAccepted.play();
  setCallTokens({
    audioToken: tokens.audioToken,
    videoToken: tokens.videoToken,
    cameraToken: tokens.cameraToken,
    participant: tokens.participant,
    timeStarted: new Date(),
    hasAudioEnabled: false,
    hasCameraEnabled: false,
    role: ParticipantRole.NONE,
    isRemoteControlEnabled: true,
    isRoomCall: !!room,
    room: room ?? undefined,
    participants: [],
    isInitialisingCall: true,
    micLevel: 0,
  });

  try {
    await typedInvoke("call_started", {
      audioToken: tokens.audioToken,
      videoToken: tokens.videoToken,
      forceMuted: true,
    });
  } catch {
    socketService.send({ type: "call_end", payload: { participant_id: tokens.participant } });
    tauriUtils.endCallCleanup();
    setCallTokens(null);
    throw new Error("Hopp could not start the call on that computer");
  }
}

async function join(payload: RemoteCommandPayload): Promise<string> {
  const { callTokens, user } = useStore.getState();

  if (payload.room_id) {
    if (callTokens?.room?.id === payload.room_id) return "Already in that room";
    if (callTokens) throw new Error("Already in another call. Use Leave call first.");

    const rooms = await api<Room[]>("GET", "/api/auth/rooms");
    const room = rooms.find((candidate) => candidate.id === payload.room_id);
    if (!room) throw new Error("That room does not exist any more");
    const tokens = await api<CallTokens>("GET", `/api/auth/room/${encodeURIComponent(room.id)}`);
    await startCallMuted(tokens, room);
    return `Joined the room "${room.name}" with microphone and camera off`;
  }

  if (callTokens) throw new Error("Already in a call. Use Leave call first.");
  if (user && user.id === payload.from_user_id) {
    throw new Error("Pick a room: this is your own account, there is no other call to join");
  }
  let tokens: CallTokens;
  try {
    tokens = await api<CallTokens>("POST", `/api/auth/call/join/${encodeURIComponent(payload.from_user_id)}`);
  } catch (error) {
    const reason = error instanceof Error ? error.message : "";
    throw new Error(
      reason.includes("not in a call") ?
        "You are not in a call yourself. Start or join one in your Hopp app first, or pick a room."
      : reason || "Could not join the call",
    );
  }
  await startCallMuted(tokens, tokens.room ?? null);
  return "Joined your call with microphone and camera off";
}

async function waitForShare(): Promise<void> {
  let failed = false;
  const unlisten = await listen("core_screenshare_failed", () => {
    failed = true;
  });
  try {
    await typedInvoke("start_default_screenshare");
    const deadline = Date.now() + SHARE_START_TIMEOUT_MS;
    while (Date.now() < deadline) {
      if (failed) throw new Error("Hopp could not start the screen share on that computer");
      const { callTokens } = useStore.getState();
      if (!callTokens) throw new Error("The call ended before the screen share started");
      if (callTokens.role === ParticipantRole.SHARER) return;
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
    throw new Error("The screen share did not start in time");
  } finally {
    unlisten();
  }
}

async function showMainWindowQuietly() {
  try {
    const main = await WebviewWindow.getByLabel("main");
    await main?.show();
    await main?.unminimize();
  } catch (error) {
    console.error("remote command: failed to show the main window", error);
  }
}

async function execute(payload: RemoteCommandPayload, endCall: () => void): Promise<string> {
  const { callTokens, setCallTokens } = useStore.getState();
  const { setRemoteShareBy } = useRemoteShareStore.getState();

  switch (payload.command) {
    case "join":
      return join(payload);

    case "leave":
      if (!callTokens) return "Was not in a call";
      setRemoteShareBy(null);
      endCall();
      return "Left the call";

    case "start_share":
      if (!callTokens) throw new Error("Not in a call yet. Use Join first.");
      if (callTokens.isInitialisingCall) throw new Error("Still connecting to the call. Try again in a few seconds.");
      if (callTokens.role === ParticipantRole.SHARER) return "The screen is already being shared";
      await waitForShare();
      setRemoteShareBy(payload.from_name || "a teammate");
      void showMainWindowQuietly();
      return "Screen share started";

    case "stop_share":
      if (!callTokens || callTokens.role !== ParticipantRole.SHARER) return "The screen was not being shared";
      setCallTokens({ ...callTokens, role: ParticipantRole.NONE, isRemoteControlEnabled: true });
      await tauriUtils.stopSharing();
      setRemoteShareBy(null);
      return "Screen share stopped";

    case "restart":
      // Answer first, then leave the call cleanly and restart the whole app.
      setTimeout(async () => {
        try {
          if (useStore.getState().callTokens) await endCallAndWait(endCall);
        } catch (error) {
          console.error("remote command: ending the call before the restart failed", error);
        }
        await typedInvoke("restart_app");
      }, 500);
      return "Restarting Hopp. It is back online in a few seconds.";

    default:
      throw new Error("This Hopp build does not know that command");
  }
}

const announcements: Record<string, string> = {
  join: "made Hopp join a call",
  leave: "made Hopp leave the call",
  start_share: "started sharing your screen",
  stop_share: "stopped your screen share",
  restart: "is restarting Hopp",
};

let commandRunning = false;

async function handleRemoteCommand(payload: RemoteCommandPayload, endCall: () => void) {
  if (!payload || typeof payload.command_id !== "string" || typeof payload.command !== "string") return;

  const reply = (ok: boolean, message: string) =>
    socketService.send({
      type: "remote_command_result",
      payload: { command_id: payload.command_id, ok, message, status: statusSnapshot() },
    });

  // The Rust side owns the decision (local switch + local allow-list) and the log line.
  const { user } = useStore.getState();
  let allowed = false;
  try {
    allowed = await typedInvoke("authorize_remote_command", {
      fromUserId: String(payload.from_user_id ?? ""),
      fromName: String(payload.from_name ?? ""),
      command: payload.command,
      isSelf: !!user && user.id === payload.from_user_id,
    });
  } catch (error) {
    console.error("remote command: authorization check failed", error);
  }
  if (!allowed) {
    reply(false, "Remote control is switched off on that computer, or you are not on its list");
    return;
  }

  if (payload.command === "status") {
    reply(true, "");
    return;
  }

  if (Math.abs(Date.now() / 1000 - Number(payload.issued_at)) > MAX_COMMAND_AGE_S) {
    reply(false, "Ignored: the command is too old, or the clock on that computer is wrong by several minutes");
    return;
  }
  if (commandRunning) {
    reply(false, "Busy with another remote command. Try again in a moment.");
    return;
  }

  commandRunning = true;
  const who = payload.from_name || "A teammate";
  try {
    const message = await execute(payload, endCall);
    toast(`Remote control: ${who} ${announcements[payload.command] ?? "sent a command"}`, { duration: 8000 });
    reply(true, message);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    console.error(`remote command ${payload.command} from ${who} failed:`, message);
    reply(false, message);
  } finally {
    commandRunning = false;
  }
}

/**
 * Wires remote commands into the main window. Does nothing visible until the
 * user switches the feature on in Settings → Remote access.
 */
export function useRemoteCommands() {
  const endCall = useEndCall();
  const endCallRef = useRef(endCall);
  endCallRef.current = endCall;

  const authToken = useStore((state) => state.authToken);
  const socketConnected = useStore((state) => state.socketConnected);

  // Mirror the local setting to the server: at sign-in, on every reconnect and on change.
  useEffect(() => {
    if (!isTauri() || !authToken || !socketConnected) return;
    const push = () => {
      pushSettingsToServer().catch((error) => console.warn("remote commands: could not sync the setting:", error));
    };
    push();
    const unlisten = listen("remote_commands_settings_changed", push);
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [authToken, socketConnected]);

  useEffect(() => {
    socketService.on("remote_command", (data: { type?: string; payload?: RemoteCommandPayload }) => {
      if (data?.type !== "remote_command" || !data.payload) return;
      void handleRemoteCommand(data.payload, () => endCallRef.current());
    });
    return () => socketService.removeHandler("remote_command");
  }, []);
}

/** Stays in the main window for as long as a remotely started screen share is running. */
export function RemoteShareBanner() {
  const remoteShareBy = useRemoteShareStore((state) => state.remoteShareBy);
  const setRemoteShareBy = useRemoteShareStore((state) => state.setRemoteShareBy);
  const callTokens = useStore((state) => state.callTokens);
  const setCallTokens = useStore((state) => state.setCallTokens);
  const sharing = callTokens?.role === ParticipantRole.SHARER;

  useEffect(() => {
    if (remoteShareBy && !sharing) setRemoteShareBy(null);
  }, [remoteShareBy, sharing, setRemoteShareBy]);

  if (!remoteShareBy || !sharing || !callTokens) return null;

  return (
    <div
      role="alert"
      className="mx-3 mt-3 rounded-md border border-amber-400 bg-amber-100 px-3 py-2 text-xs text-amber-900 flex flex-col gap-2"
    >
      <span>
        <strong>Your screen is being shared.</strong> Started remotely by {remoteShareBy}.
      </span>
      <button
        type="button"
        className="self-start rounded border border-amber-600 bg-white px-2 py-1 font-medium text-amber-900 hover:bg-amber-50"
        onClick={() => {
          setCallTokens({ ...callTokens, role: ParticipantRole.NONE, isRemoteControlEnabled: true });
          tauriUtils.stopSharing();
          setRemoteShareBy(null);
        }}
      >
        Stop sharing
      </button>
    </div>
  );
}
