import { StrictMode, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import { spots } from "./data/spots";
import type { CatchReport, FilterTag, FishingSpot } from "./types";
import "./styles.css";

const storageKey = "fremont-fishing-reports-v1";

const filters: { id: FilterTag | "all"; label: string }[] = [
  { id: "all", label: "All" },
  { id: "fishable", label: "Fishable" },
  { id: "not-fishable", label: "Not fishable" },
  { id: "family", label: "Family" },
  { id: "pier", label: "Pier" },
  { id: "stocked", label: "Stocked trout/catfish" },
  { id: "no-district-permit", label: "No district permit" },
  { id: "bay", label: "Bay/saltwater" },
  { id: "warnings", label: "Warnings" },
];

function loadReports(): CatchReport[] {
  try {
    const raw = localStorage.getItem(storageKey);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((report): report is CatchReport => {
      return (
        typeof report === "object" &&
        report !== null &&
        "spotId" in report &&
        "rating" in report &&
        typeof report.spotId === "string" &&
        typeof report.rating === "number"
      );
    });
  } catch {
    return [];
  }
}

function scoreFor(spot: FishingSpot, reports: CatchReport[]): number {
  const spotReports = reports.filter((report) => report.spotId === spot.id);
  if (spotReports.length === 0) return spot.baselineScore;
  const userAverage =
    (spotReports.reduce((sum, report) => sum + report.rating, 0) / spotReports.length) * 20;
  const confidence = Math.min(0.62, spotReports.length / 8);
  return Math.round(spot.baselineScore * (1 - confidence) + userAverage * confidence);
}

function statusLabel(status: FishingSpot["status"]): string {
  if (status === "fishable") return "Fishable";
  if (status === "not-fishable") return "Not fishable";
  return "Verify rules";
}

function App() {
  const [reports, setReports] = useState<CatchReport[]>(loadReports);
  const [query, setQuery] = useState("");
  const [activeFilter, setActiveFilter] = useState<FilterTag | "all">("all");
  const [selectedId, setSelectedId] = useState(spots[0].id);

  const ranked = useMemo(() => {
    return spots
      .map((spot) => ({ spot, score: scoreFor(spot, reports) }))
      .filter(({ spot }) => {
        const haystack = [
          spot.name,
          spot.area,
          spot.summary,
          spot.species.join(" "),
          spot.amenities.join(" "),
          spot.cautions.join(" "),
        ]
          .join(" ")
          .toLowerCase();
        const matchesQuery = haystack.includes(query.trim().toLowerCase());
        const matchesFilter = activeFilter === "all" || spot.tags.includes(activeFilter);
        return matchesQuery && matchesFilter;
      })
      .sort((a, b) => b.score - a.score);
  }, [activeFilter, query, reports]);

  const selected = spots.find((spot) => spot.id === selectedId) ?? spots[0];
  const selectedScore = scoreFor(selected, reports);
  const selectedReports = reports.filter((report) => report.spotId === selected.id);
  const comparison = ranked.slice(0, 3);

  function saveReport(report: CatchReport) {
    const nextReports = [report, ...reports];
    setReports(nextReports);
    localStorage.setItem(storageKey, JSON.stringify(nextReports));
  }

  return (
    <main className="app-shell">
      <section className="topbar">
        <div>
          <p className="eyebrow">Fremont, California</p>
          <h1>Fishing holes ranked for a real local trip</h1>
        </div>
        <div className="quick-stats" aria-label="Directory stats">
          <Stat label="Spots" value={spots.length.toString()} />
          <Stat label="Reports" value={reports.length.toString()} />
          <Stat label="Top score" value={Math.max(...ranked.map((item) => item.score), 0).toString()} />
        </div>
      </section>

      <section className="advisory" aria-label="Current conditions advisory">
        <strong>Verify before you go.</strong> Public source notes are not real-time conditions. Check
        current rules, closures, California license requirements, water quality, fish consumption
        advisories, and posted signs.
      </section>

      <section className="controls" aria-label="Search and filters">
        <label className="search-label">
          Search
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Try trout, pier, Niles, family..."
          />
        </label>
        <div className="filter-row">
          {filters.map((filter) => (
            <button
              className={activeFilter === filter.id ? "chip active" : "chip"}
              key={filter.id}
              onClick={() => setActiveFilter(filter.id)}
              type="button"
            >
              {filter.label}
            </button>
          ))}
        </div>
      </section>

      <section className="main-grid">
        <div className="rank-list" aria-label="Ranked fishing spots">
          {ranked.map(({ spot, score }, index) => (
            <button
              type="button"
              key={spot.id}
              className={selected.id === spot.id ? "spot-card selected" : "spot-card"}
              onClick={() => setSelectedId(spot.id)}
            >
              <span className="rank">#{index + 1}</span>
              <span className={`status ${spot.status}`}>{statusLabel(spot.status)}</span>
              <span className="score">{score}</span>
              <span className="spot-title">{spot.name}</span>
              <span className="spot-area">{spot.area}</span>
              <span className="spot-summary">{spot.summary}</span>
              <span className="mini-tags">
                {spot.tags.slice(0, 4).map((tag) => (
                  <span key={tag}>{tag.replaceAll("-", " ")}</span>
                ))}
              </span>
            </button>
          ))}
        </div>

        <SpotDetail
          reports={selectedReports}
          score={selectedScore}
          spot={selected}
          onSubmit={saveReport}
        />
      </section>

      <Comparison items={comparison} reports={reports} />
    </main>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <span>{value}</span>
      <small>{label}</small>
    </div>
  );
}

function SpotDetail({
  spot,
  score,
  reports,
  onSubmit,
}: {
  spot: FishingSpot;
  score: number;
  reports: CatchReport[];
  onSubmit: (report: CatchReport) => void;
}) {
  const [rating, setRating] = useState(4);
  const [species, setSpecies] = useState("");
  const [bait, setBait] = useState("");
  const [date, setDate] = useState(new Date().toISOString().slice(0, 10));
  const [crowd, setCrowd] = useState<CatchReport["crowd"]>("Moderate");
  const [note, setNote] = useState("");

  function submit(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    onSubmit({
      id: crypto.randomUUID(),
      spotId: spot.id,
      rating,
      species: species.trim() || "Not specified",
      bait: bait.trim() || "Not specified",
      date,
      crowd,
      note: note.trim(),
      createdAt: new Date().toISOString(),
    });
    setSpecies("");
    setBait("");
    setNote("");
  }

  return (
    <article className="detail-panel">
      <div className="detail-head">
        <div>
          <p className="eyebrow">{spot.area}</p>
          <h2>{spot.name}</h2>
          <p>{spot.summary}</p>
        </div>
        <div className="detail-score">
          <span>{score}</span>
          <small>blended score</small>
        </div>
      </div>

      <LocationGraphic spot={spot} />

      <div className="detail-columns">
        <InfoBlock title="Expected Species" items={spot.species.length ? spot.species : ["No fishing target"]} />
        <InfoBlock title="Amenities" items={spot.amenities} />
        <InfoBlock title="Cautions" items={spot.cautions} />
      </div>

      <dl className="facts">
        <div>
          <dt>Permit</dt>
          <dd>{spot.permitNotes}</dd>
        </div>
        <div>
          <dt>Access</dt>
          <dd>{spot.accessNotes}</dd>
        </div>
        <div>
          <dt>Coordinates</dt>
          <dd>
            {spot.coordinates.lat.toFixed(4)}, {spot.coordinates.lng.toFixed(4)}
          </dd>
        </div>
      </dl>

      <section className="source-box">
        <h3>Public Source Notes</h3>
        <ul>
          {spot.publicSourceNotes.map((sourceNote) => (
            <li key={sourceNote}>{sourceNote}</li>
          ))}
        </ul>
        <div className="source-links">
          {spot.sources.map((source) => (
            <a key={source.url} href={source.url} target="_blank" rel="noreferrer">
              {source.label}
            </a>
          ))}
        </div>
      </section>

      <form className="report-form" onSubmit={submit}>
        <h3>Rate or add a catch report</h3>
        <div className="form-grid">
          <label>
            Rating
            <input
              min="1"
              max="5"
              type="number"
              value={rating}
              onChange={(event) => setRating(Number(event.target.value))}
            />
          </label>
          <label>
            Species
            <input value={species} onChange={(event) => setSpecies(event.target.value)} />
          </label>
          <label>
            Bait
            <input value={bait} onChange={(event) => setBait(event.target.value)} />
          </label>
          <label>
            Date
            <input type="date" value={date} onChange={(event) => setDate(event.target.value)} />
          </label>
          <label>
            Crowd
            <select value={crowd} onChange={(event) => setCrowd(event.target.value as CatchReport["crowd"])}>
              <option>Quiet</option>
              <option>Moderate</option>
              <option>Busy</option>
            </select>
          </label>
        </div>
        <label>
          Note
          <textarea value={note} onChange={(event) => setNote(event.target.value)} rows={3} />
        </label>
        <button className="primary" type="submit">
          Save report
        </button>
      </form>

      <section className="reports">
        <h3>Community reports</h3>
        {reports.length === 0 ? (
          <p>No reports yet for this spot.</p>
        ) : (
          reports.map((report) => (
            <article key={report.id} className="report">
              <strong>{report.rating}/5</strong>
              <span>
                {report.date} · {report.species} · {report.bait} · {report.crowd}
              </span>
              {report.note ? <p>{report.note}</p> : null}
            </article>
          ))
        )}
      </section>
    </article>
  );
}

function InfoBlock({ title, items }: { title: string; items: string[] }) {
  return (
    <section className="info-block">
      <h3>{title}</h3>
      <ul>
        {items.map((item) => (
          <li key={item}>{item}</li>
        ))}
      </ul>
    </section>
  );
}

function LocationGraphic({ spot }: { spot: FishingSpot }) {
  const minLat = 37.49;
  const maxLat = 37.59;
  const minLng = -122.13;
  const maxLng = -121.95;
  const left = ((spot.coordinates.lng - minLng) / (maxLng - minLng)) * 100;
  const top = 100 - ((spot.coordinates.lat - minLat) / (maxLat - minLat)) * 100;

  return (
    <div className="map-graphic" aria-label={`Schematic location for ${spot.name}`}>
      <span className="bay-label">Bay</span>
      <span className="city-label">Fremont</span>
      <span className="trail-line" />
      <span className="marker" style={{ left: `${left}%`, top: `${top}%` }} />
    </div>
  );
}

function Comparison({
  items,
  reports,
}: {
  items: { spot: FishingSpot; score: number }[];
  reports: CatchReport[];
}) {
  return (
    <section className="comparison">
      <div className="section-head">
        <p className="eyebrow">Side-by-side</p>
        <h2>Top spot comparison</h2>
      </div>
      <div className="compare-grid">
        {items.map(({ spot, score }) => (
          <article key={spot.id}>
            <div className="compare-title">
              <h3>{spot.name}</h3>
              <span>{score}</span>
            </div>
            <p><strong>Permit:</strong> {spot.permitNotes}</p>
            <p><strong>Species:</strong> {spot.species.slice(0, 5).join(", ") || "Not fishable"}</p>
            <p><strong>Access:</strong> {spot.accessNotes}</p>
            <p><strong>Amenities:</strong> {spot.amenities.slice(0, 5).join(", ")}</p>
            <p><strong>Cautions:</strong> {spot.cautions[0]}</p>
            <small>{reports.filter((report) => report.spotId === spot.id).length} saved reports</small>
          </article>
        ))}
      </div>
    </section>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
