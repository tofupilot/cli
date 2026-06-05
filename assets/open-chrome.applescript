(*
 * Focus an existing tab matching the URL, or open a new one. Returns
 * "focused" / "opened" / "not_running" on stdout — the caller checks
 * the string, NOT the exit code (osascript returns 0 even when the
 * inner `tell` block soft-fails).
 *
 * Args: <url> <browser app name>
 *
 * Notes on the AppleScript shape:
 *   * `tell application <variable>` requires a `using terms from`
 *     anchor so the compiler can resolve the verb dictionary;
 *     "Google Chrome" stands in here because every Chromium-family
 *     browser ships the same scripting dictionary.
 *   * `if exists (processes whose name is X)` is the canonical
 *     "is the app running?" test — checking via `application X is
 *     running` would launch the app under modern macOS.
 *)
on run argv
	set targetURL to item 1 of argv
	set browserName to item 2 of argv

	tell application "System Events"
		set isRunning to (exists (processes whose name is browserName))
	end tell
	if not isRunning then return "not_running"

	using terms from application "Google Chrome"
		tell application browserName
			repeat with w in windows
				set tabIndex to 1
				repeat with t in tabs of w
					if URL of t starts with targetURL then
						set active tab index of w to tabIndex
						set index of w to 1
						activate
						return "focused"
					end if
					set tabIndex to tabIndex + 1
				end repeat
			end repeat
			open location targetURL
			activate
			return "opened"
		end tell
	end using terms from
end run
