/**
 * Settings → Remote access (fork feature, see src/lib/remote-commands.tsx).
 *
 * The switch and the allow-list live in this app's own settings file; the main
 * window mirrors them to the server whenever they change.
 */
import { useState } from "react";
import { Checkbox } from "@/components/ui/checkbox";
import { Button } from "@/components/ui/button";
import { typedInvoke, type UserSettings } from "@/core_payloads";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import useStore from "@/store/store";
import { Constants } from "@/constants";
import { FORK_BUILD_LABEL } from "@/lib/fork-build";

export function RemoteAccessSettings({ settings, onChanged }: { settings: UserSettings; onChanged: () => void }) {
  const teammates = useStore((state) => state.teammates);
  const user = useStore((state) => state.user);
  // Read the store value so the link follows a changed Custom Backend URL.
  useStore((state) => state.customServerUrl);
  const [copied, setCopied] = useState(false);

  const enabled = settings.remote_commands_enabled;
  const allowed = settings.remote_commands_allowed_users ?? [];
  const controlPageUrl = `https://${Constants.apiBaseUrl}/remote-control`;
  const people = [...(teammates ?? [])].sort((a, b) =>
    `${a.first_name} ${a.last_name}`.localeCompare(`${b.first_name} ${b.last_name}`),
  );

  const setAllowed = async (users: string[]) => {
    await typedInvoke("set_remote_commands_allowed_users", { users });
    onChanged();
  };

  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-col gap-3">
        <h3 className="text-base font-medium text-black dark:text-white">Remote access</h3>
        <label className="flex items-start gap-2 cursor-pointer">
          <Checkbox
            className="mt-0.5 rounded border-gray-300 dark:border-gray-600"
            checked={enabled}
            onCheckedChange={async (value) => {
              await typedInvoke("set_remote_commands_enabled", { enabled: value === true });
              onChanged();
            }}
          />
          <div className="flex flex-col">
            <span className="text-sm font-medium text-gray-700 dark:text-gray-300">
              Allow trusted people to control my Hopp remotely
            </span>
            <span className="text-sm text-gray-500 dark:text-gray-400">
              Off by default. The people you tick below can then, from a web page and without any click on this
              computer: start and stop your screen share, make this Hopp join or leave a call, and restart Hopp. Every
              time you get a notification, and it is written to the log.
            </span>
          </div>
        </label>
      </div>

      {enabled && (
        <>
          <hr className="h-px w-full border-none bg-gray-300 dark:bg-gray-600" />

          <div className="flex flex-col gap-3">
            <h3 className="text-base font-medium text-black dark:text-white">Who is allowed</h3>
            {!user && (
              <p className="text-sm text-gray-500 dark:text-gray-400">Sign in to Hopp to see your teammates.</p>
            )}
            {user && people.length === 0 && (
              <p className="text-sm text-gray-500 dark:text-gray-400">
                You have no teammates on this server yet. Invite the person to your team first.
              </p>
            )}
            {people.map((person) => (
              <label key={person.id} className="flex items-start gap-2 cursor-pointer">
                <Checkbox
                  className="mt-0.5 rounded border-gray-300 dark:border-gray-600"
                  checked={allowed.includes(person.id)}
                  onCheckedChange={(value) => {
                    const known = allowed.filter((id) => people.some((candidate) => candidate.id === id));
                    void setAllowed(
                      value === true ?
                        [...known.filter((id) => id !== person.id), person.id]
                      : known.filter((id) => id !== person.id),
                    );
                  }}
                />
                <div className="flex flex-col">
                  <span className="text-sm font-medium text-gray-700 dark:text-gray-300">
                    {person.first_name} {person.last_name}
                  </span>
                  <span className="text-sm text-gray-500 dark:text-gray-400">{person.email}</span>
                </div>
              </label>
            ))}
            <p className="text-sm text-gray-500 dark:text-gray-400">
              Nobody else can do this, not even other teammates. Your own account can always use the page for this
              computer while the switch is on.
            </p>
          </div>

          <hr className="h-px w-full border-none bg-gray-300 dark:bg-gray-600" />

          <div className="flex flex-col gap-2">
            <h3 className="text-base font-medium text-black dark:text-white">The page they open</h3>
            <p className="text-sm text-gray-500 dark:text-gray-400">
              They sign in there with their normal Hopp account. Any Hopp version works for them, the page runs in a
              browser.
            </p>
            <div className="flex items-center gap-2">
              <code className="flex-1 min-w-0 truncate rounded bg-gray-100 px-2 py-1 text-xs text-gray-800 dark:bg-gray-800 dark:text-gray-200">
                {controlPageUrl}
              </code>
              <Button
                size="sm"
                variant="outline"
                onClick={async () => {
                  await writeText(controlPageUrl);
                  setCopied(true);
                  setTimeout(() => setCopied(false), 2000);
                }}
              >
                {copied ? "Copied" : "Copy link"}
              </Button>
            </div>
          </div>

          <hr className="h-px w-full border-none bg-gray-300 dark:bg-gray-600" />

          <div className="flex flex-col gap-2">
            <h3 className="text-base font-medium text-black dark:text-white">Good to know</h3>
            <ul className="list-disc pl-5 text-sm text-gray-500 dark:text-gray-400 flex flex-col gap-1">
              <li>
                A screen share started this way shares your whole main screen (or the screen you shared last since Hopp
                was started). The person can also use your mouse and keyboard, exactly as in a normal share, unless you
                switch off "Enable remote control by default" in Call settings.
              </li>
              <li>When Hopp joins a call this way, your microphone and camera stay off.</li>
              <li>
                While a remotely started share is running, the Hopp window shows a banner with a Stop button. Switch
                this setting off to stop all remote access at once.
              </li>
            </ul>
          </div>
        </>
      )}

      <p className="text-xs text-gray-400 dark:text-gray-500">Build {FORK_BUILD_LABEL}</p>
    </div>
  );
}
