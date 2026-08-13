import alfredMark from "./assets/alfred-mark.png";

export function AlfredLogo({ size = 32, className }: { size?: number; className?: string }) {
  return (
    <img
      src={alfredMark}
      width={size}
      height={size}
      alt=""
      draggable={false}
      className={className ?? "alfred-logo"}
    />
  );
}
