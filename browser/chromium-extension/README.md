# Chromium browser bridge

This unpacked Manifest V3 extension provides semantic DOM observation and actions for Chrome, Edge, Brave, and other Chromium browsers. It talks only to the `com.alfred.browser_bridge` Native Messaging host; webpages cannot access the bridge. Install this folder through the browser's **Load unpacked** control, then install the generated native-host manifest for that browser.

Supported commands are `status`, `listTabs`, `navigate`, `captureVisible`, `observe`, `click`, and `type`. Element references expire whenever the page changes. The extension independently blocks destructive language and typing into password fields.
