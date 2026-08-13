export function parseHttpUrl(value) {
  try {
    const url = new URL(value);
    if (url.protocol !== "http:" && url.protocol !== "https:") return null;
    return url;
  } catch {
    return null;
  }
}

export function urlPath(value) {
  const url = parseHttpUrl(value);
  if (!url) return "";
  return `${url.origin}${url.pathname.replace(/\/$/, "")}`;
}

export function urlMatches(actual, expected) {
  const a = parseHttpUrl(actual);
  const e = parseHttpUrl(expected);
  if (!a || !e) return false;
  if (urlPath(actual) !== urlPath(expected)) return false;
  if (e.hash && a.hash !== e.hash) return false;
  return true;
}

export function navigationSucceeded(startUrl, currentUrl, expectedUrl, committedUrl) {
  if (urlMatches(currentUrl, expectedUrl)) return true;
  const startPath = urlPath(startUrl);
  const nowPath = urlPath(currentUrl);
  if (!nowPath) return false;
  const leftStart = Boolean(startPath && nowPath !== startPath);
  const sawCommit = Boolean(committedUrl && urlPath(committedUrl) && urlPath(committedUrl) !== startPath);
  return leftStart && sawCommit;
}
