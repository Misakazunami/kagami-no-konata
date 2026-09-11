import { useEffect, useState } from "react";

const WEEKDAYS = ["日", "一", "二", "三", "四", "五", "六"];

function formatTime(date: Date): string {
  const h = date.getHours().toString().padStart(2, "0");
  const m = date.getMinutes().toString().padStart(2, "0");
  return `${h}:${m}`;
}

function formatDate(date: Date): string {
  const month = date.getMonth() + 1;
  const day = date.getDate();
  const weekday = WEEKDAYS[date.getDay()];
  return `${month}月${day}日 周${weekday}`;
}

export function FloatingClock() {
  const [now, setNow] = useState(() => new Date());

  useEffect(() => {
    const timer = setInterval(() => setNow(new Date()), 1000);
    return () => clearInterval(timer);
  }, []);

  return (
    <div className="float-clock">
      <div className="float-clock-time">{formatTime(now)}</div>
      <div className="float-clock-date">{formatDate(now)}</div>
    </div>
  );
}
