import { parseHttpUrl, urlMatches, navigationSucceeded } from "./urls.js";

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

assert(parseHttpUrl("https://x.com/compose/post"), "https compose URL should parse");
assert(!parseHttpUrl("file:///C:/Windows/System32/cmd.exe"), "file URL must be rejected");
assert(!parseHttpUrl("javascript:alert(1)"), "javascript URL must be rejected");

assert(
  urlMatches("https://x.com/compose/post", "https://x.com/compose/post/"),
  "trailing slash should not break a path match"
);
assert(
  !urlMatches("https://x.com/home", "https://x.com/compose/post"),
  "same origin is not a path match"
);
assert(
  urlMatches("https://x.com/compose/post#draft", "https://x.com/compose/post"),
  "an extra hash is fine when none was requested"
);
assert(
  !urlMatches("https://x.com/compose/post", "https://x.com/compose/post#pane"),
  "a requested hash is part of the destination"
);

assert(
  !navigationSucceeded(
    "https://x.com/home",
    "https://x.com/home",
    "https://x.com/compose/post",
    null
  ),
  "SPA stall on the start URL is not success"
);
assert(
  navigationSucceeded(
    "https://x.com/home",
    "https://x.com/compose/post",
    "https://x.com/compose/post",
    "https://x.com/compose/post"
  ),
  "requested path after a committed change is success"
);
assert(
  navigationSucceeded(
    "https://mail.google.com/mail/u/0/#inbox",
    "https://mail.google.com/mail/u/0/#sent",
    "https://mail.google.com/mail/u/0/#sent",
    "https://mail.google.com/mail/u/0/#sent"
  ),
  "same-path hash destinations should match"
);
assert(
  navigationSucceeded(
    "https://example.com/a",
    "https://example.com/login",
    "https://example.com/settings",
    "https://example.com/login"
  ),
  "a committed same-origin redirect away from the start path is success"
);

console.log("PASS browser URL matching");
