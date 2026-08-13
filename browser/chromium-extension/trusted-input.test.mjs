function viewportBox(rect, innerWidth, innerHeight) {
  const width = Math.max(1, innerWidth || 0);
  const height = Math.max(1, innerHeight || 0);
  const nx = (rect.x + rect.width / 2) / width;
  const ny = (rect.y + rect.height / 2) / height;
  if (!Number.isFinite(nx) || !Number.isFinite(ny) || nx < 0 || nx > 1 || ny < 0 || ny > 1) {
    return null;
  }
  return { nx: Number(nx.toFixed(4)), ny: Number(ny.toFixed(4)), space: "page" };
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const box = viewportBox({ x: 100, y: 50, width: 40, height: 20 }, 800, 600);
assert(box && box.space === "page", "page-space box should be returned");
assert(box.nx === 0.15, `expected 0.15 nx, got ${box.nx}`);
assert(box.ny === 0.1, `expected 0.1 ny, got ${box.ny}`);
assert(viewportBox({ x: -40, y: 10, width: 10, height: 10 }, 800, 600) === null, "offscreen left is rejected");
assert(viewportBox({ x: 790, y: 10, width: 40, height: 10 }, 800, 600) === null, "offscreen right is rejected");

const clickResult = { clicked: false, needsTrustedInput: true, reason: "contenteditable", ...box };
assert(clickResult.needsTrustedInput === true, "contenteditable should request trusted input");
assert(typeof clickResult.nx === "number" && typeof clickResult.ny === "number", "trusted click must carry a box");

console.log("PASS trusted input viewport boxes");
