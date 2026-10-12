# ADR-0004: Explicit desktop Electron transport

Status: Accepted for local desktop extension

Use the installed ChatGPT.app main Electron renderer through local CDP. Launch a
separate app instance with a dedicated profile through macOS LaunchServices.
Select only the main app origin; never fall back to a similarly titled browser,
overlay or auxiliary app page. Retain browser operation as an explicit backend.

Desktop authentication stays with the user-owned client. Do not clone a live
profile or export credentials. Fail dispatch when Chat is not the active composer.
Native desktop selectors remain behind the existing BrowserPort, preserving the
application's terminal and recovery policies. Verified model selection and multi-task endpoint leases are now implemented
under the desktop workbench contract; application owns supervision policy.
