import {
  Bar, BarChart, CartesianGrid, Legend, Line, LineChart, ReferenceLine,
  ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";
import { gib, mib, Peak, Sample } from "../api";

const TIP_STYLE = { background: "var(--card)", border: "1px solid var(--border)" };

// Peak RAM/disk per job, across all jobs.
export function PeaksChart({ peaks }: { peaks: Peak[] }) {
  const data = peaks.map((p) => ({ id: `#${p.id}`, RAM: +gib(p.ram_peak), Disk: +gib(p.disk_peak) }));
  return (
    <ResponsiveContainer width="100%" height={180}>
      <BarChart data={data}>
        <CartesianGrid strokeDasharray="3 3" stroke="#8b949e" strokeOpacity={0.3} />
        <XAxis dataKey="id" stroke="#8b949e" /><YAxis stroke="#8b949e" unit="G" />
        <Tooltip contentStyle={TIP_STYLE} />
        <Legend /><Bar dataKey="RAM" fill="#58a6ff" /><Bar dataKey="Disk" fill="#bc8cff" />
      </BarChart>
    </ResponsiveContainer>
  );
}

// RAM/disk over the run, with phase markers dropped on the time axis. Labels are
// staggered onto lower rows so close-together phases don't overlap.
export function ResourceChart(
  { samples, t0, phaseTs }: { samples: Sample[]; t0: number; phaseTs: Record<string, number> },
) {
  const data = samples.map((s) => ({
    t: Math.max(0, Math.round(((s.ts_ms ?? (s as any).ts_ms) - t0) / 1000)),
    RAM: +mib(s.rss_bytes ?? (s as any).rss),
    Disk: +mib(s.disk_bytes ?? (s as any).disk),
  }));
  const marks = Object.entries(phaseTs)
    .map(([phase, ts]) => ({ phase, t: Math.max(0, Math.round((ts - t0) / 1000)) }))
    .sort((a, b) => a.t - b.t);
  const maxT = data.length ? data[data.length - 1].t : 0;
  const GAP = Math.max(1, maxT * 0.13); // ponytail: ~label-width as a fraction of the
                                        // time axis; bump if labels still touch
  const rowLastT: number[] = [];
  const marksRows = marks.map((m) => {
    let r = rowLastT.findIndex((last) => m.t - last >= GAP);
    if (r === -1) r = rowLastT.length;
    rowLastT[r] = m.t;
    return { ...m, row: r };
  });

  return (
    <ResponsiveContainer width="100%" height={220}>
      <LineChart data={data}>
        <CartesianGrid strokeDasharray="3 3" stroke="#8b949e" strokeOpacity={0.3} />
        <XAxis dataKey="t" type="number" domain={[0, "dataMax"]} stroke="#8b949e" unit="s" />
        <YAxis stroke="#8b949e" unit="M" />
        <Tooltip contentStyle={TIP_STYLE} />
        <Legend />
        {marksRows.map((m) => (
          <ReferenceLine key={m.phase} x={m.t} stroke="#8b949e" strokeDasharray="4 3"
            label={({ viewBox }: any) => {
              const nearRight = maxT > 0 && m.t > maxT * 0.85;
              const x = (viewBox.x as number) + (nearRight ? -3 : 3);
              const y = (viewBox.y as number) + 4 + m.row * 12; // +4 clears the top Y tick
              return (
                <text x={x} y={y} fill="#8b949e" fontSize={10}
                  textAnchor={nearRight ? "end" : "start"} dominantBaseline="hanging">
                  {m.phase}
                </text>
              );
            }} />
        ))}
        <Line type="monotone" dataKey="RAM" stroke="#58a6ff" dot={false} isAnimationActive={false} />
        <Line type="monotone" dataKey="Disk" stroke="#bc8cff" dot={false} isAnimationActive={false} />
      </LineChart>
    </ResponsiveContainer>
  );
}
