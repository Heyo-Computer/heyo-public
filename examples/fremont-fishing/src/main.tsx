import React, { FormEvent, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  AlertTriangle,
  CalendarDays,
  Check,
  ExternalLink,
  Fish,
  MapPin,
  Search,
  SlidersHorizontal,
  Star,
  Waves,
} from "lucide-react";
import { filterOptions, FishingSpot, fishingSpots, sourceCatalog } from "./spots";
import "./styles.css";

type Report = {
  id: string;
  spotId: string;
  rating: number;
  species: string;
  bait: string;
  date: string;
  crowd: string;
  note: string;
};

type DraftReport = Omit<Report, "id" | "spotId">;

const storageKey = "fremont-fishing-reports";

function readReports(): Report[] {
  try {
    const raw = localStorage.getItem(storageKey);
    return raw ? (JSON.parse(raw) as Report[]) : [];
  } catch {
    return [];
  }
}

function scoreFor(spot: FishingSpot, reports: Report[]) {
  const spotReports = reports.filter((report) => report.spotId === spot.id);
  if (spotReports.length === 0) return spot.baselineScore;
  const userAverage =
    spotReports.reduce((sum, report) => sum + report.rating, 0) / spotReports.length;
  const blended = spot.baselineScore * 0.72 + userAverage * 2 * 0.28;
  return Math.min(9.8, Number(blended.toFixed(1)));
}

function stars(value: number) {
  return Array.from({ length: 5 }, (_, index) => (
    <Star
      key={index}
      size={15}
      className={index < Math.round(value) ? "star is-on" : "star"}
      aria-hidden="true"
    />
  ));
}

function App() {
  const [reports, setReports] = useState<Report[]>(readReports);
  const [selectedId, setSelectedId] = useState(fishingSpots[0].id);
  const [query, setQuery] = useState("");
  const [activeFilters, setActiveFilters] = useState<string[]>(["fishable"]);
  const [draft, setDraft] = useState<DraftReport>({
    rating: 4,
    species: "",
    bait: "",
    date: new Date().toISOString().slice(0, 10),
    crowd: "Moderate",
    note: "",
  });

  const ranked = useMemo(() => {
    return fishingSpots
      .map((spot) => ({
        ...spot,
        score: scoreFor(spot, reports),
        reportCount: reports.filter((report) => report.spotId === spot.id).length,
      }))
      .sort((a, b) => b.score - a.score);
  }, [reports]);

  const visibleSpots = ranked.filter((spot) => {
    const haystack = `${spot.name} ${spot.area} ${spot.summary} ${spot.expectedSpecies.join(" ")}`.toLowerCase();
    const matchesQuery = haystack.includes(query.trim().toLowerCase());
    const matchesFilters = activeFilters.every((filter) => spot.tags.includes(filter));
    return matchesQuery && matchesFilters;
  });

  const selected = ranked.find((spot) => spot.id === selectedId) ?? ranked[0];
  const selectedReports = reports
    .filter((report) => report.spotId === selected.id)
    .sort((a, b) => b.date.localeCompare(a.date));
  const fishableCount = fishingSpots.filter((spot) => spot.fishable).length;
  const userReportCount = reports.length;

  function toggleFilter(filter: string) {
    setActiveFilters((current) =>
      current.includes(filter) ? current.filter((item) => item !== filter) : [...current, filter],
    );
  }

  function submitReport(event: FormEvent) {
    event.preventDefault();
    const report: Report = {
      ...draft,
      id: crypto.randomUUID(),
      spotId: selected.id,
      species: draft.species.trim() || "Not specified",
      bait: draft.bait.trim() || "Not specified",
      note: draft.note.trim(),
    };
    const next = [report, ...reports];
    localStorage.setItem(storageKey, JSON.stringify(next));
    setReports(next);
    setDraft({
      rating: 4,
      species: "",
      bait: "",
      date: new Date().toISOString().slice(0, 10),
      crowd: "Moderate",
      note: "",
    });
  }

  return (
    <main className="app-shell">
      <section className="topbar">
        <div>
          <p className="kicker">Fremont, CA</p>
          <h1>Fishing Holes</h1>
        </div>
        <div className="top-stats" aria-label="Directory stats">
          <Stat label="fishable spots" value={fishableCount.toString()} />
          <Stat label="public sources" value={sourceCatalog.length.toString()} />
          <Stat label="user reports" value={userReportCount.toString()} />
        </div>
      </section>

      <section className="controls" aria-label="Search and filters">
        <label className="search-box">
          <Search size={18} aria-hidden="true" />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search species, place, access..."
          />
        </label>
        <div className="filter-row">
          <SlidersHorizontal size={18} aria-hidden="true" />
          {filterOptions.map((filter) => (
            <button
              key={filter.id}
              type="button"
              className={activeFilters.includes(filter.id) ? "chip is-active" : "chip"}
              onClick={() => toggleFilter(filter.id)}
            >
              {filter.label}
            </button>
          ))}
        </div>
      </section>

      <section className="layout">
        <div className="rank-list" aria-label="Ranked fishing spots">
          {visibleSpots.map((spot, index) => (
            <button
              key={spot.id}
              type="button"
              className={spot.id === selected.id ? "spot-card is-selected" : "spot-card"}
              onClick={() => setSelectedId(spot.id)}
            >
              <span className="rank">#{index + 1}</span>
              <span className="spot-main">
                <span className="spot-name">{spot.name}</span>
                <span className="spot-area">{spot.area}</span>
                <span className="spot-summary">{spot.summary}</span>
                <span className="tag-row">
                  {spot.bestFor.slice(0, 4).map((tag) => (
                    <span key={tag}>{tag}</span>
                  ))}
                </span>
              </span>
              <span className="score-box">
                <strong>{spot.score.toFixed(1)}</strong>
                <small>{spot.reportCount ? `${spot.reportCount} reports` : "source score"}</small>
              </span>
            </button>
          ))}
        </div>

        <article className="detail-panel">
          <div className="detail-head">
            <div>
              <p className={selected.fishable ? "status is-open" : "status is-closed"}>
                {selected.fishable ? "Fishable public-source entry" : "Not fishable warning"}
              </p>
              <h2>{selected.name}</h2>
              <p>{selected.area}</p>
            </div>
            <div className="big-score">
              <strong>{selected.score.toFixed(1)}</strong>
              <span>rank score</span>
            </div>
          </div>

          <MapGraphic selected={selected} />

          <div className="info-grid">
            <InfoBlock icon={<Fish size={18} />} title="Expected Fish" value={selected.expectedSpecies.join(", ") || "None listed"} />
            <InfoBlock icon={<Check size={18} />} title="Permit Notes" value={selected.permitNotes} />
            <InfoBlock icon={<MapPin size={18} />} title="Access" value={selected.access} />
            <InfoBlock icon={<Waves size={18} />} title="Amenities" value={selected.amenities.join(", ")} />
          </div>

          <section className="compare-panel">
            <h3>Quick Compare</h3>
            <div className="compare-grid">
              {ranked.slice(0, 5).map((spot) => (
                <button
                  type="button"
                  key={spot.id}
                  className={spot.id === selected.id ? "compare-item is-selected" : "compare-item"}
                  onClick={() => setSelectedId(spot.id)}
                >
                  <span>{spot.name}</span>
                  <strong>{spot.score.toFixed(1)}</strong>
                  <small>{spot.permitNotes}</small>
                </button>
              ))}
            </div>
          </section>

          <section className="report-panel">
            <div className="section-title">
              <h3>Rate This Spot</h3>
              <span>{stars(draft.rating)}</span>
            </div>
            <form onSubmit={submitReport}>
              <label>
                Rating
                <input
                  type="range"
                  min="1"
                  max="5"
                  value={draft.rating}
                  onChange={(event) => setDraft({ ...draft, rating: Number(event.target.value) })}
                />
              </label>
              <div className="form-row">
                <label>
                  Species
                  <input
                    value={draft.species}
                    onChange={(event) => setDraft({ ...draft, species: event.target.value })}
                    placeholder="striped bass"
                  />
                </label>
                <label>
                  Bait
                  <input
                    value={draft.bait}
                    onChange={(event) => setDraft({ ...draft, bait: event.target.value })}
                    placeholder="anchovy"
                  />
                </label>
              </div>
              <div className="form-row">
                <label>
                  Date
                  <input
                    type="date"
                    value={draft.date}
                    onChange={(event) => setDraft({ ...draft, date: event.target.value })}
                  />
                </label>
                <label>
                  Crowd
                  <select
                    value={draft.crowd}
                    onChange={(event) => setDraft({ ...draft, crowd: event.target.value })}
                  >
                    <option>Quiet</option>
                    <option>Moderate</option>
                    <option>Busy</option>
                  </select>
                </label>
              </div>
              <label>
                Note
                <textarea
                  value={draft.note}
                  onChange={(event) => setDraft({ ...draft, note: event.target.value })}
                  placeholder="Water clarity, bite window, parking, wind..."
                />
              </label>
              <button type="submit" className="primary-button">
                Save report
              </button>
            </form>
          </section>

          <section className="report-list">
            <h3>Recent Reports</h3>
            {selectedReports.length === 0 ? (
              <p className="empty">No community reports yet. Add the first one for this spot.</p>
            ) : (
              selectedReports.map((report) => (
                <div key={report.id} className="report-item">
                  <div>
                    <strong>{report.species}</strong>
                    <span>{stars(report.rating)}</span>
                  </div>
                  <p>{report.note || `Bait: ${report.bait}. Crowd: ${report.crowd}.`}</p>
                  <small>
                    <CalendarDays size={13} aria-hidden="true" /> {report.date} · {report.crowd} · {report.bait}
                  </small>
                </div>
              ))
            )}
          </section>

          <section className="source-panel">
            <div>
              <h3>Public Source Notes</h3>
              <ul>
                {selected.sourceNotes.map((note) => (
                  <li key={note}>{note}</li>
                ))}
              </ul>
            </div>
            <div>
              <h3>Sources</h3>
              <div className="source-links">
                {selected.sources.map((source) => (
                  <a href={source.url} key={source.url} target="_blank" rel="noreferrer">
                    {source.label}
                    <ExternalLink size={13} aria-hidden="true" />
                  </a>
                ))}
              </div>
            </div>
          </section>

          <aside className="advisory">
            <AlertTriangle size={18} aria-hidden="true" />
            <span>
              Verify current rules, closures, licenses, consumption advisories, water quality, and posted signs before going. User reports are local notes, not official guidance.
            </span>
          </aside>
        </article>
      </section>
    </main>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="stat">
      <strong>{value}</strong>
      <span>{label}</span>
    </div>
  );
}

function InfoBlock({ icon, title, value }: { icon: React.ReactNode; title: string; value: string }) {
  return (
    <div className="info-block">
      <span>{icon}</span>
      <div>
        <strong>{title}</strong>
        <p>{value}</p>
      </div>
    </div>
  );
}

function MapGraphic({ selected }: { selected: FishingSpot & { score: number; reportCount: number } }) {
  const left = ((selected.coordinates[1] + 122.13) / 0.19) * 100;
  const top = ((37.59 - selected.coordinates[0]) / 0.1) * 100;

  return (
    <div className="map-graphic" aria-label={`Approximate map marker for ${selected.name}`}>
      <svg viewBox="0 0 640 220" role="img">
        <title>Stylized Fremont waterways map</title>
        <path d="M25 58 C135 34 207 108 314 76 C419 45 507 44 616 88" className="map-water" />
        <path d="M34 160 C150 131 202 195 320 154 C438 113 489 147 606 126" className="map-trail" />
        <path d="M98 78 L188 130 L270 102 L382 148 L516 82" className="map-road" />
        <circle cx="424" cy="102" r="42" className="map-lake" />
        <circle cx="365" cy="128" r="22" className="map-lake small" />
        <circle cx="282" cy="122" r="15" className="map-pond" />
      </svg>
      <span className={selected.fishable ? "map-pin" : "map-pin is-warning"} style={{ left: `${left}%`, top: `${top}%` }}>
        <MapPin size={20} aria-hidden="true" />
      </span>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
