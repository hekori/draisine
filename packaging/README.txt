DRAISINE: safe sync of local folders with Google Drive & co. (via rclone)
=========================================================================

Start
-----
  Linux:    ./draisine           (best added to autostart)
  Windows:  draisine.exe
  macOS:    Draisine.app         (first start: right-click > Open, the app is not signed)

A Draisine icon appears in the tray / menu bar:
  grey   = auto-sync off          green  = auto-sync on, all good
  blue   = sync running           orange = changes waiting for your confirmation
  red    = error (see log)

Left-click opens the window, right-click the menu.
GNOME needs the extension "AppIndicator and KStatusNotifierItem Support".

First steps
-----------
1. On first start the setup assistant opens. It checks rclone, lets you sign in to
   your cloud storage in the browser and choose a local folder and a cloud folder.
   An rclone that is already set up is used automatically.
2. "Compare" (in the "•••" menu of a folder pair) shows the differences. Nothing is changed.
3. "Sync" shows a preview; only "Apply" changes anything.

Safety
------
- Before anything is deleted or overwritten you are asked (preview + confirmation).
- Files deleted or overwritten locally go to the trash.
- Conflicts (file changed on both sides): both versions are kept as
    name-YYYY-MM-DDTHH-MM-SSZ-local.ext  and  name-YYYY-MM-DDTHH-MM-SSZ-remote.ext
- Auto-sync only does what cannot destroy anything (new files, conflicts). Deletes and
  overwrites wait until you confirm them (the icon turns orange).
- If one side is suddenly empty (not mounted, wrong path), the sync stops.

Files in this folder
--------------------
  draisine / draisine.exe   the app
  rclone / rclone.exe       the transfer tool (https://rclone.org), used automatically
  rclone.conf               (optional) your own rclone configuration, used automatically
  draisine-data/            (optional) create it for PORTABLE mode: settings, sync state
                            and logs are then stored here instead of in your user profile.

Logs: menu > "Open log (TXT)". Logs are never deleted automatically.
