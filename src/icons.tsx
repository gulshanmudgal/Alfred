import type { ReactNode } from "react";

interface IconProps {
  name: string;
  size?: number;
  strokeWidth?: number;
}

const paths: Record<string, ReactNode> = {
  home: <><path d="m3 11 9-8 9 8"/><path d="M5 10v10h14V10"/><path d="M9 20v-6h6v6"/></>,
  workflow: <><rect x="3" y="3" width="6" height="6" rx="2"/><rect x="15" y="15" width="6" height="6" rx="2"/><path d="M9 6h4a4 4 0 0 1 4 4v5"/><path d="m14 12 3 3 3-3"/></>,
  runs: <><path d="M8 5v14l11-7Z"/><circle cx="12" cy="12" r="10"/></>,
  settings: <><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.88l.06.06-2.83 2.83-.06-.06A1.7 1.7 0 0 0 15 19.4a1.7 1.7 0 0 0-1 .6 1.7 1.7 0 0 0-.4 1v.1h-4v-.1a1.7 1.7 0 0 0-1.1-1.6 1.7 1.7 0 0 0-1.88.34l-.06.06-2.83-2.83.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-.6-1 1.7 1.7 0 0 0-1-.4h-.1v-4H3A1.7 1.7 0 0 0 4.6 8.5a1.7 1.7 0 0 0-.34-1.88l-.06-.06 2.83-2.83.06.06A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1-.6 1.7 1.7 0 0 0 .4-1v-.1h4V3a1.7 1.7 0 0 0 1.1 1.6 1.7 1.7 0 0 0 1.88-.34l.06-.06 2.83 2.83-.06.06A1.7 1.7 0 0 0 19.4 9c.12.37.34.7.6 1 .28.3.64.43 1 .4h.1v4H21a1.7 1.7 0 0 0-1.6.6Z"/></>,
  plus: <><path d="M12 5v14M5 12h14"/></>,
  search: <><circle cx="11" cy="11" r="7"/><path d="m20 20-4-4"/></>,
  shield: <><path d="M12 22s8-4 8-11V5l-8-3-8 3v6c0 7 8 11 8 11Z"/><path d="m9 12 2 2 4-5"/></>,
  folder: <><path d="M3 6h6l2 2h10v11H3Z"/><path d="M3 6v13"/></>,
  brain: <><path d="M9.5 4.5A3.5 3.5 0 0 0 6 8a3 3 0 0 0 .5 5.9A3.5 3.5 0 0 0 10 19h2V5.5a3 3 0 0 0-2.5-1Z"/><path d="M14.5 4.5A3.5 3.5 0 0 1 18 8a3 3 0 0 1-.5 5.9A3.5 3.5 0 0 1 14 19h-2M8 9h4m-5 5h5m4-5h-4m5 5h-5"/></>,
  monitor: <><rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8m-4-4v4"/></>,
  check: <path d="m5 12 4 4L19 6"/>,
  arrow: <path d="m9 18 6-6-6-6"/>,
  pause: <><path d="M9 5v14M15 5v14"/></>,
  stop: <rect x="6" y="6" width="12" height="12" rx="2"/>,
  hand: <><path d="M18 11V7a2 2 0 0 0-4 0v3-5a2 2 0 0 0-4 0v5-3a2 2 0 0 0-4 0v6l-1-1a2 2 0 0 0-3 3l5 6h8a6 6 0 0 0 6-6v-4a2 2 0 0 0-4 0"/></>,
  sparkle: <><path d="m12 3 1.2 3.8L17 8l-3.8 1.2L12 13l-1.2-3.8L7 8l3.8-1.2Z"/><path d="m19 14 .7 2.3L22 17l-2.3.7L19 20l-.7-2.3L16 17l2.3-.7Z"/></>,
  clock: <><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></>,
  lock: <><rect x="4" y="10" width="16" height="11" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/></>,
  link: <><path d="M10 13a5 5 0 0 0 7.5.5l2-2a5 5 0 0 0-7-7l-1.2 1.2"/><path d="M14 11a5 5 0 0 0-7.5-.5l-2 2a5 5 0 0 0 7 7l1.2-1.2"/></>,
  chevron: <path d="m6 9 6 6 6-6"/>,
  close: <path d="M18 6 6 18M6 6l12 12"/>,
};

export function Icon({ name, size = 20, strokeWidth = 1.8 }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={strokeWidth} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      {paths[name] ?? paths.sparkle}
    </svg>
  );
}
