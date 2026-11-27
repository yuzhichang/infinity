package loadtest

// End-to-end load test against a running Infinity server.
//
// This is the executable form of docs/load_test_plan_ragflow_prod.md: it walks
// P0 (create) -> P1 (ingest) -> P3/P4/P5 (query phases) -> P7 (readers plus
// writers) -> P8 (teardown), and prints a report per phase. Every phase is
// gated by an environment variable so the cheap ones can run in CI and the L
// scenario can be started deliberately:
//
//	LT_SERVER=127.0.0.1:28317 \
//	LT_MAPPING=/path/to/ragflow/conf/infinity_mapping.json \
//	LT_MODEL=/path/to/model2vec-1024 \
//	LT_SCENARIO=S \
//	go test -count=1 -v -timeout 6h -run TestLoadTest ./...
//
// LT_SCENARIO may be S / M / L / XL, or "S:10x200" for a custom table count and
// rows per table. LT_TABLES / LT_ROWS override either.

import (
	"context"
	"fmt"
	"math/rand"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	infinity "github.com/infiniflow/infinity-go-sdk"
)

// config is everything the run needs, read from the environment.
type config struct {
	Server      string
	Mapping     string
	Model       string
	Scenario    Scenario
	Seed        int64
	Queries     int
	Sparsegram  bool
	Skew        bool
	IndexLast   bool // P1b: load first, then build indexes
	Connections int  // fixed number of Thrift clients
	LimitTables int  // 0 = all tables in the scenario
}

func loadConfig(t *testing.T) config {
	t.Helper()
	server := os.Getenv("LT_SERVER")
	if server == "" {
		t.Skip("set LT_SERVER=host:port to run the load test")
	}
	mapping := os.Getenv("LT_MAPPING")
	if mapping == "" {
		t.Fatal("LT_MAPPING must point at RAGFlow's conf/infinity_mapping.json")
	}
	model := os.Getenv("LT_MODEL")
	if model == "" {
		t.Fatal("LT_MODEL must point at the pinned model2vec model directory")
	}

	scName := getenvDefault("LT_SCENARIO", "S")
	sc, err := parseScenario(scName)
	if err != nil {
		t.Fatalf("LT_SCENARIO: %v", err)
	}
	if v := os.Getenv("LT_TABLES"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil {
			t.Fatalf("LT_TABLES: %v", err)
		}
		sc.Tables = n
		if os.Getenv("LT_ROWS") == "" {
			sc.Rows = n * 1000
		}
	}
	if v := os.Getenv("LT_ROWS"); v != "" {
		n, err := strconv.Atoi(v)
		if err != nil {
			t.Fatalf("LT_ROWS: %v", err)
		}
		sc.Rows = n
	}

	return config{
		Server:      server,
		Mapping:     mapping,
		Model:       model,
		Scenario:    sc,
		Seed:        int64(getenvInt("LT_SEED", 20261124)),
		Queries:     getenvInt("LT_QUERIES", 200),
		Sparsegram:  os.Getenv("LT_SPARSEGRAM") != "",
		Skew:        os.Getenv("LT_SKEW") != "",
		IndexLast:   os.Getenv("LT_INDEX_LAST") != "",
		Connections: connectionCount(sc.Name, runtime.NumCPU()),
		// LimitTables trims the plan to the first N tables. It exists so a run
		// fits inside a window where the machine is not reclaimed underneath it;
		// it is not a way to measure a smaller workload (the per-table shape is
		// unchanged, so the numbers stay comparable).
		LimitTables: getenvInt("LT_LIMIT_TABLES", 0),
	}
}

// connectionCount is how many Thrift clients a scenario opens: one per
// connection, each blocking on its batch's response before sending the next.
//
// The multiplier follows the table count, because the engine's work per insert
// is mostly per table (the catalog lookups and the index tail are per table,
// the row encoding is per row) and a run with more tables keeps more of them
// live at once. Measured on L: 32 clients (2x) left the server at 6.25 of 16
// cores with sys+us at 46%, so the clients were the limit, not the engine.
//
// S/M/L/XL = 1/2/4/8.
func connectionCount(scenario string, cpus int) int {
	base := strings.SplitN(scenario, "/", 2)[0]
	multiplier := 1
	switch base {
	case "M":
		multiplier = 2
	case "L":
		multiplier = 4
	case "XL":
		multiplier = 8
	}
	return max(1, cpus) * multiplier
}

func TestConnectionCount(t *testing.T) {
	const cpus = 8
	cases := []struct {
		scenario string
		want     int
	}{
		{"S", cpus},
		{"M", cpus * 2},
		{"L", cpus * 4},
		{"L/100", cpus * 4},
		{"XL", cpus * 8},
		{"XL/100", cpus * 8},
		{"S:10x200", cpus},
		{"M:10x200", cpus * 2},
		{"L:10x200", cpus * 4},
	}
	for _, tc := range cases {
		sc, err := parseScenario(tc.scenario)
		if err != nil {
			t.Fatalf("parseScenario(%q): %v", tc.scenario, err)
		}
		if got := connectionCount(sc.Name, cpus); got != tc.want {
			t.Errorf("connectionCount(%q, %d) = %d, want %d", sc.Name, cpus, got, tc.want)
		}
	}
	if got := connectionCount("L", 0); got != 4 {
		t.Errorf("connectionCount(\"L\", 0) = %d, want 4", got)
	}
}

// parseScenario accepts "S", "M", "L", "XL" or "S:10x200".
//
// A trailing scale is also accepted: "L/100" means the L scenario with both the
// table count and the row count divided by 100, so a smoke run keeps the same
// shape (rows per table, rows per document) at 1% of the size. That matters
// because the interesting properties here are per-table and per-row ratios, not
// the totals — scaling only the row count would collapse every table to a
// handful of rows and measure something else.
func parseScenario(s string) (Scenario, error) {
	scale := 1
	if i := strings.LastIndex(s, "/"); i >= 0 {
		v := s[i+1:]
		n, err := strconv.Atoi(v)
		if err != nil || n < 1 {
			return Scenario{}, fmt.Errorf("bad scale in %q: expected <name>/<divisor>", s)
		}
		scale = n
		s = s[:i]
	}

	sc, custom, err := parseScenarioBase(s)
	if err != nil {
		return Scenario{}, err
	}
	if scale > 1 {
		// Custom "NxM" shapes already say exactly what they want; only the named
		// scenarios get scaled, and the original name is kept with the factor so
		// a report cannot be mistaken for a full-size run.
		if custom {
			return Scenario{}, fmt.Errorf("scale is only for named scenarios (S/M/L/XL), not %q", s)
		}
		sc.Tables = max(1, sc.Tables/scale)
		sc.Rows = max(1, sc.Rows/scale)
		sc.Name = fmt.Sprintf("%s/%d", sc.Name, scale)
	}
	return sc, nil
}

func parseScenarioBase(s string) (Scenario, bool, error) {
	if base := strings.SplitN(s, ":", 2); len(base) == 2 {
		var tables, rows int
		if _, err := fmt.Sscanf(base[1], "%dx%d", &tables, &rows); err != nil {
			return Scenario{}, false, fmt.Errorf("expected <name>:<tables>x<rows>, got %q", s)
		}
		return Scenario{Name: base[0], Tables: tables, Rows: tables * rows}, true, nil
	}
	sc, ok := Scenarios[s]
	if !ok {
		return Scenario{}, false, fmt.Errorf("unknown scenario %q (want S/M/L/XL, optionally with a /scale, or <name>:<tables>x<rows>)", s)
	}
	return sc, false, nil
}

func getenvDefault(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}

func getenvInt(k string, d int) int {
	if v := os.Getenv(k); v != "" {
		n, err := strconv.Atoi(v)
		if err == nil {
			return n
		}
	}
	return d
}

// TestLoadTest is the single entry point. It is a test rather than a main so
// `go test -timeout` bounds a hung run and `-run` selects phases.
func TestLoadTest(t *testing.T) {
	cfg := loadConfig(t)
	run := &Run{Cfg: cfg, T: t}
	run.Execute()
}

// PinnedMappingSHA256 is the authoritative RAGFlow conf/infinity_mapping.json hash (plan §4.1.1).
const PinnedMappingSHA256 = "d779cf10185bdfea89ef787c4f7643c8a12269df7096faab82d1bf86037f4532"

// Run holds the state a run accumulates, so the report can be assembled at the
// end even when a phase fails.
type Run struct {
	Cfg config
	T   *testing.T

	dbName        string
	mapping       *Mapping
	mappingSHA    string
	modelHashes   map[string]string
	effectiveSkew bool
	corpus        *Corpus
	emb           *Embedder
	alloc         Allocation
	engine        *Engine
	clients       []*Engine

	createdMu     sync.Mutex
	createdTables []string
	teardownDone  bool

	report []phaseResult
}

func (r *Run) trackCreated(name string) {
	r.createdMu.Lock()
	r.createdTables = append(r.createdTables, name)
	r.createdMu.Unlock()
}

// phaseResult is one measured phase.
type phaseResult struct {
	Phase   string
	Seconds float64
	Rows    int
	Detail  string
}

func (r *Run) record(phase string, dur time.Duration, rows int, detail string) {
	r.report = append(r.report, phaseResult{Phase: phase, Seconds: dur.Seconds(), Rows: rows, Detail: detail})
	r.T.Logf("[%s] %.2fs %s", phase, dur.Seconds(), detail)
}

// Execute walks the phases in order. A failure in a setup phase stops the run;
// a failure inside a query phase is recorded and the run continues, because the
// point of the phase is the distribution, not a single query.
func (r *Run) Execute() {
	t := r.T

	// ---- setup -----------------------------------------------------------
	start := time.Now()
	m, err := LoadMapping(r.Cfg.Mapping)
	if err != nil {
		t.Fatalf("load mapping: %v", err)
	}
	if m.SHA256 != PinnedMappingSHA256 {
		t.Fatalf("mapping sha256 mismatch: %s != %s", m.SHA256, PinnedMappingSHA256)
	}
	r.mapping = m
	r.mappingSHA = m.SHA256

	arts, sha, err := LoadCorpus(wikiPath(), PinnedWikiSHA256)
	if err != nil {
		t.Fatalf("load corpus: %v", err)
	}
	r.corpus = NewCorpus(arts)

	modelHashes, err := ValidatePinnedModel(r.Cfg.Model)
	if err != nil {
		t.Fatalf("validate model: %v", err)
	}
	r.modelHashes = modelHashes

	emb, err := LoadEmbedder(r.Cfg.Model, 1024)
	if err != nil {
		t.Fatalf("load embedder: %v", err)
	}
	defer emb.Close()
	r.emb = emb

	r.effectiveSkew = r.Cfg.Skew && r.Cfg.Scenario.Tables >= 200
	if r.Cfg.Skew && !r.effectiveSkew {
		t.Logf("LT_SKEW=1 requested, but scenario %s has %d tables (< 200); using uniform allocation",
			r.Cfg.Scenario.Name, r.Cfg.Scenario.Tables)
	}
	r.alloc = PlanAllocation(r.Cfg.Scenario, r.Cfg.Seed, r.effectiveSkew, "ragflow_lt")
	if n := r.Cfg.LimitTables; n > 0 && n < len(r.alloc.Rows) {
		r.alloc.Rows = r.alloc.Rows[:n]
		r.alloc.Docs = r.alloc.Docs[:n]
		r.alloc.Names = r.alloc.Names[:n]
		t.Logf("LT_LIMIT_TABLES=%d: plan trimmed to the first %d tables", n, n)
	}
	t.Logf("scenario %s: %s (skew=%v)", r.Cfg.Scenario.Name, r.alloc.Summary(), r.effectiveSkew)
	t.Logf("corpus: %d articles, %d start positions, sha256 %s", len(arts), r.corpus.Starts(), sha[:16])
	t.Logf("schema: %d mapping columns + 1 vector, %d index objects (variant=%s), sha256 %s",
		len(m.Columns), m.IndexObjects(r.Cfg.Sparsegram), variantName(r.Cfg.Sparsegram), r.mappingSHA[:16])

	host, port, err := splitHostPort(r.Cfg.Server)
	if err != nil {
		t.Fatalf("LT_SERVER: %v", err)
	}
	r.dbName = fmt.Sprintf("lt_load_%d_%d", r.Cfg.Seed, time.Now().UnixNano())
	eng, err := Connect(host, port, r.dbName, m, 1024)
	if err != nil {
		t.Fatalf("connect: %v", err)
	}
	r.engine = eng
	r.clients = append(r.clients, eng)
	defer func() {
		r.cleanupOnExit()
		for _, client := range r.clients {
			client.Close()
		}
	}()
	for len(r.clients) < r.Cfg.Connections {
		client, err := Connect(host, port, r.dbName, m, 1024)
		if err != nil {
			t.Fatalf("connect client %d/%d: %v", len(r.clients)+1, r.Cfg.Connections, err)
		}
		r.clients = append(r.clients, client)
	}
	t.Logf("Thrift clients: %d (Go NumCPU=%d, scenario=%s)", len(r.clients), runtime.NumCPU(), r.Cfg.Scenario.Name)
	r.record("setup", time.Since(start), 0, "mapping+corpus+embedder+clients")

	// ---- P0: create tables ----------------------------------------------
	r.phaseCreate()
	if os.Getenv("LT_CREATE_ONLY") != "" {
		r.phaseTeardown()
		r.printReport()
		return
	}

	// ---- P1: ingest ------------------------------------------------------
	r.phaseIngest()

	// ---- P2: index (only in the load-then-index order) -------------------
	if r.Cfg.IndexLast {
		r.phaseIndex()
	}

	// ---- P2b: seal the indexes ------------------------------------------
	r.phaseDumpIndex()

	// ---- queries --------------------------------------------------------
	queries, err := BuildQueries(rand.New(rand.NewSource(r.Cfg.Seed)), r.corpus, r.alloc, r.emb, r.Cfg.Queries)
	if err != nil {
		t.Fatalf("build queries: %v", err)
	}
	// Exercise each kind against the same query set, so the difference between
	// the numbers is the query shape and nothing else.
	kinds := []string{"dense", "text", "hybrid", "filtered", "like", "point"}
	if os.Getenv("LT_PROBE_INDEX") != "" {
		// Attribution runs: the index path, the index path without the filter,
		// and the no-index control. Not part of the production mix.
		kinds = []string{"dense", "denseidx", "scan"}
	}
	for _, k := range kinds {
		r.phaseQueries(k, withKind(queries, k))
	}

	// ---- P7: readers plus writers ---------------------------------------
	r.phaseMixed(queries)

	// ---- P8: teardown ---------------------------------------------------
	r.phaseTeardown()

	r.printReport()
}

func variantName(sparsegram bool) string {
	if sparsegram {
		return "+sparsegram"
	}
	return "default"
}

func wikiPath() string {
	if p := os.Getenv("LT_WIKI"); p != "" {
		return p
	}
	return "../../test/data/benchmark/enwiki/enwiki-10w.csv"
}

func splitHostPort(s string) (string, int, error) {
	i := strings.LastIndex(s, ":")
	if i < 0 {
		return "", 0, fmt.Errorf("expected host:port, got %q", s)
	}
	port, err := strconv.Atoi(s[i+1:])
	if err != nil {
		return "", 0, fmt.Errorf("bad port in %q: %w", s, err)
	}
	return s[:i], port, nil
}

// phaseCreate runs P0: CREATE TABLE plus every index, before any row exists.
//
// Existing tables with this scenario's names are dropped first: a rerun has to
// be idempotent, and CREATE TABLE with ConflictTypeError would otherwise fail on
// the second run against the same database (which is how this was found).
func (r *Run) phaseCreate() {
	if os.Getenv("LT_CLEAN_STALE") != "" {
		r.dropExisting()
	}

	start := time.Now()
	// Step 1: CREATE TABLE in parallel over the fixed client pool.
	if len(r.alloc.Names) > 0 {
		if len(r.clients) == 0 {
			r.T.Fatal("no Thrift clients")
		}
		workers := min(len(r.clients), len(r.alloc.Names))
		jobs := make(chan string, len(r.alloc.Names))
		for _, name := range r.alloc.Names {
			jobs <- name
		}
		close(jobs)
		var wg sync.WaitGroup
		errCh := make(chan error, len(r.alloc.Names))
		for w := 0; w < workers; w++ {
			wg.Add(1)
			go func(worker int) {
				defer wg.Done()
				eng := r.clients[worker]
				for name := range jobs {
					if _, err := eng.CreateTable(name, false, r.Cfg.Sparsegram); err != nil {
						errCh <- fmt.Errorf("create table %s: %w", name, err)
						return
					}
					r.trackCreated(name)
				}
			}(w)
		}
		wg.Wait()
		close(errCh)
		for err := range errCh {
			r.T.Fatalf("create table: %v", err)
		}
	}
	tableDuration := time.Since(start)
	r.T.Logf("P0 tables: %d tables in %.2fs (%.1f tables/s)", len(r.alloc.Names), tableDuration.Seconds(), float64(len(r.alloc.Names))/tableDuration.Seconds())
	indexStart := time.Now()
	// Step 2: CREATE INDEX in parallel over the same client pool.
	if !r.Cfg.IndexLast && len(r.alloc.Names) > 0 {
		if len(r.clients) == 0 {
			r.T.Fatal("no Thrift clients")
		}
		workers := min(len(r.clients), len(r.alloc.Names))
		jobs := make(chan string, len(r.alloc.Names))
		for _, name := range r.alloc.Names {
			jobs <- name
		}
		close(jobs)

		var wg sync.WaitGroup
		errCh := make(chan error, len(r.alloc.Names))
		for w := 0; w < workers; w++ {
			wg.Add(1)
			go func(worker int) {
				defer wg.Done()
				eng := r.clients[worker]
				for name := range jobs {
					tbl, err := eng.Table(name)
					if err != nil {
						errCh <- fmt.Errorf("get table %s: %w", name, err)
						return
					}
					if err := eng.CreateIndexes(tbl, r.Cfg.Sparsegram); err != nil {
						errCh <- fmt.Errorf("create indexes %s: %w", name, err)
						return
					}
				}
			}(w)
		}
		wg.Wait()
		close(errCh)
		for err := range errCh {
			r.T.Fatalf("create indexes: %v", err)
		}
		indexDuration := time.Since(indexStart)
		count := len(r.alloc.Names) * r.mapping.IndexObjects(r.Cfg.Sparsegram)
		r.T.Logf("P0 indexes: %d indexes in %.2fs (%.1f indexes/s)", count, indexDuration.Seconds(), float64(count)/indexDuration.Seconds())
	}
	dur := time.Since(start)
	n := len(r.alloc.Names)
	r.record("P0 create", dur, 0, fmt.Sprintf("%d tables x %d columns + %d indexes, %d clients, %.1f ms/table",
		n, len(r.mapping.Columns)+1, r.mapping.IndexObjects(r.Cfg.Sparsegram), len(r.clients),
		float64(dur.Milliseconds())/float64(max(1, n))))
}

// dropExisting removes tables left by an earlier run, plus any P7 writer tables.
func (r *Run) dropExisting() {
	resp, err := r.engine.DB.ListTables()
	if err != nil {
		r.T.Fatalf("list tables: %v", err)
	}
	// ListTables returns an untyped thrift response; read the names out of it.
	names := tableNames(resp)
	dropped := 0
	for _, name := range names {
		if !strings.HasPrefix(name, "ragflow_lt") {
			continue
		}
		if _, err := r.engine.DB.DropTable(name, infinity.ConflictTypeIgnore); err != nil {
			r.T.Logf("drop stale table %s: %v", name, err)
			continue
		}
		dropped++
	}
	if dropped > 0 {
		r.T.Logf("dropped %d tables from an earlier run", dropped)
	}
}

// tableNames pulls TableNames out of the SDK's ListTables response without
// importing the internal thrift package (which Go forbids from outside the SDK).
func tableNames(resp interface{}) []string {
	type named interface{ GetTableNames() []string }
	if n, ok := resp.(named); ok {
		return n.GetTableNames()
	}
	// Fall back to formatting the struct and picking bracket contents, which is
	// what the SDK returns when the typed getter is absent.
	s := fmt.Sprint(resp)
	i := strings.Index(s, "TableNames:[")
	if i < 0 {
		return nil
	}
	rest := s[i+len("TableNames:["):]
	j := strings.Index(rest, "]")
	if j < 0 {
		return nil
	}
	return strings.Fields(rest[:j])
}

// phaseIngest runs P1: build rows, embed them, insert them.
//
// Rows are generated and embedded in batches on several goroutines, each with
// its own SDK connection, but each table is written by exactly one goroutine so
// the per-table row order stays reproducible. The embedder is shared and holds
// its own lock; model2vec-rs parallelises inside a call, so the win here comes
// from overlapping embed with insert, not from concurrent encodes.
func (r *Run) phaseIngest() {
	t := r.T
	totalRows := r.alloc.Total()
	start := time.Now()

	type job struct {
		idx int
	}
	jobs := make(chan job, len(r.alloc.Names))
	for i := range r.alloc.Names {
		jobs <- job{idx: i}
	}
	close(jobs)

	var inserted atomic.Int64
	// 64, not 1024: a smaller batch is what makes the per-commit cost visible.
	// The index build work per row is unchanged (`ef_construction` fixes it),
	// so the difference this exposes is everything around it - one RPC, one
	// admission check, one WAL frame and one commit per 64 rows instead of per
	// 1024.
	batchSize := getenvInt("LT_BATCH_SIZE", 64)
	var wg sync.WaitGroup
	errCh := make(chan error, len(r.alloc.Names))

	// Progress heartbeat. Without it, a run that stalls at table 100 of 600 looks
	// identical to one that is merely slow, which is exactly how a 40-minute
	// stall was first seen.
	done := make(chan struct{})
	go func() {
		tick := time.NewTicker(15 * time.Second)
		defer tick.Stop()
		for {
			select {
			case <-done:
				return
			case <-tick.C:
				n := inserted.Load()
				t.Logf("P1 progress: %d/%d rows, %.0f rows/s so far",
					n, totalRows, float64(n)/time.Since(start).Seconds())
			}
		}
	}()
	defer close(done)

	workers := min(len(r.clients), len(r.alloc.Names))
	if len(r.alloc.Names) > 0 && workers == 0 {
		t.Fatal("P1 ingest: no Thrift clients")
	}
	for w := 0; w < workers; w++ {
		wg.Add(1)
		go func(worker int) {
			defer wg.Done()
			// One pooled connection per worker; no SDK connection is shared
			// between goroutines.
			eng := r.clients[worker]

			for j := range jobs {
				idx := j.idx
				rows := r.alloc.Rows[idx]
				if rows == 0 {
					continue
				}
				tbl, err := eng.Table(r.alloc.Names[idx])
				if err != nil {
					errCh <- fmt.Errorf("get table %s: %w", r.alloc.Names[idx], err)
					return
				}
				tblStart := time.Now()
				if err := r.ingestTable(eng, tbl, idx, batchSize, &inserted); err != nil {
					errCh <- err
					return
				}
				t.Logf("P1: table %s (%d rows) took %.1fs", r.alloc.Names[idx], r.alloc.Rows[idx], time.Since(tblStart).Seconds())
			}
		}(w)
	}
	wg.Wait()
	close(errCh)
	for err := range errCh {
		t.Fatalf("ingest: %v", err)
	}

	dur := time.Since(start)
	got := int(inserted.Load())
	if got != totalRows {
		t.Errorf("ingested %d rows, want %d", got, totalRows)
	}
	r.record("P1 ingest", dur, got, fmt.Sprintf("%d rows into %d tables, %.0f rows/s, batch %d",
		got, len(r.alloc.Names), float64(got)/dur.Seconds(), getenvInt("LT_BATCH_SIZE", 64)))
}

// ingestTable fills one table.
// ingestWithBackpressure inserts one batch, waiting out the memory governor's
// refusal instead of failing the run.
//
// The engine refuses a write at its critical watermark (`admit_statement`, 90%
// of `buffer_manager_size`). That is the governor working, not an ingest
// failure: it is the mechanism that keeps the process from being killed by the
// kernel, and it clears itself once the reclaim pass has drained enough. A test
// that treated it as fatal could never run a scenario large enough to reach the
// ceiling, which on L (5884 tables, 64 connections) is exactly what happens.
//
// The retry is unbounded on purpose. A bounded one would turn "the machine is
// slower than the run assumed" into a failure, and the caller has a wall-clock
// timeout anyway; what it must not do is spin, so each attempt waits.
func ingestWithBackpressure(eng *Engine, tbl *infinity.Table, rows []Row, vectors []float32, table string) error {
	const maxWait = 30 * time.Second
	wait := 250 * time.Millisecond
	refusals := 0
	for {
		err := eng.IngestBatch(tbl, rows, vectors)
		if err == nil {
			if refusals > 0 {
				fmt.Printf("  %s: admitted after %d refusal(s)\n", table, refusals)
			}
			return nil
		}
		if !isMemoryRefusal(err) {
			return err
		}
		refusals++
		time.Sleep(wait)
		if wait < maxWait {
			wait *= 2
			if wait > maxWait {
				wait = maxWait
			}
		}
	}
}

// isMemoryRefusal reports whether `err` is the governor's admission refusal.
//
// The message is the server's (`ADMISSION_REFUSED` in the executor's memory
// module, surfaced through the Thrift error text), and it is the only signal a
// client gets: the RPC has no distinct status code for "over the ceiling, try
// again".
func isMemoryRefusal(err error) bool {
	return err != nil && strings.Contains(err.Error(), "memory limit exceeded")
}

func (r *Run) ingestTable(eng *Engine, tbl *infinity.Table, idx, batchSize int, inserted *atomic.Int64) error {
	rows := r.alloc.Rows[idx]
	docs := r.alloc.Docs[idx]
	kbID := fmt.Sprintf("kb_%05d", idx)
	docIDs := make([]string, docs)
	for i := range docIDs {
		docIDs[i] = fmt.Sprintf("%032x", rand.New(rand.NewSource(int64(idx)<<32|int64(i))).Int63())
	}

	// Per-table RNG so a table's content does not depend on which worker got it.
	rng := rand.New(rand.NewSource(r.Cfg.Seed + int64(idx)*7919))
	rb := &RowBuilder{Mapping: r.mapping, Dim: r.emb.Dim()}

	rowBatch := make([]Row, 0, batchSize)
	texts := make([]string, 0, batchSize)
	flush := func() error {
		if len(rowBatch) == 0 {
			return nil
		}
		vectors, err := r.emb.Encode(texts, fallbackTokens)
		if err != nil {
			return fmt.Errorf("embed table %s: %w", r.alloc.Names[idx], err)
		}
		if err := ingestWithBackpressure(eng, tbl, rowBatch, vectors, r.alloc.Names[idx]); err != nil {
			return err
		}
		inserted.Add(int64(len(rowBatch)))
		rowBatch = rowBatch[:0]
		texts = texts[:0]
		return nil
	}

	for i := 0; i < rows; i++ {
		art, text := r.corpus.Sample(rng)
		docID := docIDs[i%docs]
		order := i / docs
		row, err := rb.Build(rng, art, text, docID, kbID, order, 1+order/10)
		if err != nil {
			return fmt.Errorf("build row: %w", err)
		}
		rowBatch = append(rowBatch, row)
		texts = append(texts, text)
		if len(rowBatch) == batchSize {
			if err := flush(); err != nil {
				return err
			}
		}
	}
	return flush()
}

// phaseIndex is P2 for the load-then-index order: build every index after the
// rows are in.
//
// It runs single-threaded and with no readers, deliberately. A CREATE INDEX on a
// populated table segfaults the server if a SELECT is running concurrently
// (measured; see phaseMixed), so this phase must not overlap anything.
func (r *Run) phaseIndex() {
	t := r.T
	start := time.Now()
	totalRows := 0
	for i, name := range r.alloc.Names {
		if r.alloc.Rows[i] == 0 {
			continue
		}
		tbl, err := r.engine.Table(name)
		if err != nil {
			t.Fatalf("P2 get table %s: %v", name, err)
		}
		if err := r.engine.CreateIndexes(tbl, r.Cfg.Sparsegram); err != nil {
			t.Fatalf("P2 create indexes on %s: %v", name, err)
		}
		totalRows += r.alloc.Rows[i]
		if i%25 == 0 {
			t.Logf("P2: %d/%d tables indexed (%.0fs)", i+1, len(r.alloc.Names), time.Since(start).Seconds())
		}
	}
	dur := time.Since(start)
	r.record("P2 index", dur, totalRows, fmt.Sprintf("%d index objects per table over %d rows, %.1f ms/row",
		r.mapping.IndexObjects(r.Cfg.Sparsegram), totalRows, float64(dur.Milliseconds())/float64(max(1, totalRows))))
}

// phaseDumpIndex is P2b: seal each table's indexes into their chunks.
//
// Without it every query reads the in-memory tail, not the chunk: `CREATE INDEX`
// builds the graph in `hnsw_mem` and only a dump publishes it, and a query over
// a tail is a different code path (and, while a build holds the graph's
// `tokio::sync::Mutex`, a skipped one). Production dumps (`DumpMemIndex`), so a
// latency measured on tails is not the latency production sees. `LT_DUMP_INDEX=0`
// keeps the tails on purpose, to compare the two.
func (r *Run) phaseDumpIndex() {
	if os.Getenv("LT_DUMP_INDEX") == "0" {
		r.T.Log("P2b skipped: LT_DUMP_INDEX=0, queries will read the in-memory tails")
		return
	}
	t := r.T
	start := time.Now()
	dumped := 0
	for i, name := range r.alloc.Names {
		if r.alloc.Rows[i] == 0 {
			continue
		}
		tbl, err := r.engine.Table(name)
		if err != nil {
			t.Fatalf("P2b get table %s: %v", name, err)
		}
		// One dump per index: the server dumps the named index only.
		for _, info := range r.indexNames(name) {
			if _, err := tbl.DumpIndex(info); err != nil {
				t.Fatalf("P2b dump %s.%s: %v", name, info, err)
			}
			dumped++
		}
		if i%25 == 0 {
			t.Logf("P2b: %d/%d tables sealed (%.0fs)", i+1, len(r.alloc.Names), time.Since(start).Seconds())
		}
	}
	dur := time.Since(start)
	r.record("P2b dump", dur, 0, fmt.Sprintf("%d indexes sealed over %d tables, %.1f ms/index",
		dumped, len(r.alloc.Names), float64(dur.Milliseconds())/float64(max(1, dumped))))
}

// indexNames lists the index names CreateIndexes creates for a table.
func (r *Run) indexNames(table string) []string {
	var out []string
	for _, ft := range r.mapping.FullTextIndexes(r.Cfg.Sparsegram) {
		out = append(out, SafeIndexName("ft_", ft[0], ft[1]))
	}
	for _, col := range r.mapping.SecondaryIndexes() {
		out = append(out, strings.TrimSuffix(SafeIndexName("sec_", col, ""), "_"))
	}
	out = append(out, "vec_idx")
	return out
}

// withKind copies specs under another query kind, keeping text and vector.
func withKind(specs []QuerySpec, kind string) []QuerySpec {
	out := make([]QuerySpec, len(specs))
	for i, s := range specs {
		s.Kind = kind
		if kind == "point" {
			// Q5/Q6 need an id or kb, not a phrase; the driver fills them from
			// the table's own rows below.
			s.Text = ""
		}
		out[i] = s
	}
	return out
}

// phaseQueries runs one query kind across the fixed client pool and reports
// latency. The pool is opened during setup, so connection setup is not part of
// any query measurement.
func (r *Run) phaseQueries(kind string, specs []QuerySpec) {
	if kind == "point" {
		// Resolve a real id per table so the point lookup is not a miss.
		specs = r.pointSpecs(specs)
	}
	ctx := context.Background()
	start := time.Now()
	var timings []Timed
	if len(r.clients) <= 1 {
		timings = r.engine.RunSequential(ctx, specs)
	} else {
		timings = RunConcurrent(ctx, r.clients, specs)
	}
	dur := time.Since(start)

	errs := 0
	rows := 0
	lats := make([]time.Duration, 0, len(timings))
	for _, tm := range timings {
		if tm.Err != nil {
			errs++
			continue
		}
		lats = append(lats, tm.Latency)
		rows += tm.Rows
	}
	if errs > 0 {
		r.T.Errorf("Q(%s): %d/%d queries failed; first error: %v", kind, errs, len(timings), firstErr(timings))
	}
	p50, p99 := percentiles(lats)
	r.record("Q "+kind, dur, rows, fmt.Sprintf("%d queries, p50 %s p99 %s, %d errors",
		len(timings), p50.Round(time.Microsecond), p99.Round(time.Microsecond), errs))
}

// pointSpecs resolves a real doc_id or kb_id for each point query, so both
// Q5 (kb_id secondary index) and Q6 (doc_id) hit rows instead of missing.
func (r *Run) pointSpecs(specs []QuerySpec) []QuerySpec {
	out := make([]QuerySpec, 0, len(specs))
	for i, s := range specs {
		if s.Table == "" {
			continue
		}
		var idx int
		if _, err := fmt.Sscanf(s.Table, "ragflow_lt_%05d", &idx); err != nil {
			continue
		}
		if i%2 == 0 {
			s.KBID = fmt.Sprintf("kb_%05d", idx)
			s.DocID = ""
		} else {
			docs := 1
			if idx >= 0 && idx < len(r.alloc.Docs) && r.alloc.Docs[idx] > 0 {
				docs = r.alloc.Docs[idx]
			}
			docIdx := i % docs
			s.DocID = fmt.Sprintf("%032x", rand.New(rand.NewSource(int64(idx)<<32|int64(docIdx))).Int63())
			s.KBID = ""
		}
		out = append(out, s)
	}
	return out
}

func firstErr(ts []Timed) error {
	for _, t := range ts {
		if t.Err != nil {
			return t.Err
		}
	}
	return nil
}

func percentiles(lats []time.Duration) (p50, p99 time.Duration) {
	if len(lats) == 0 {
		return 0, 0
	}
	sorted := append([]time.Duration(nil), lats...)
	sortDurations(sorted)
	return sorted[len(sorted)/2], sorted[min(len(sorted)-1, len(sorted)*99/100)]
}

func sortDurations(xs []time.Duration) {
	for i := 1; i < len(xs); i++ {
		for j := i; j > 0 && xs[j] < xs[j-1]; j-- {
			xs[j], xs[j-1] = xs[j-1], xs[j]
		}
	}
}

// phaseMixed is P7: readers and writers at the same time.
//
// The writers create and fill a fresh table, which is what a tenant onboarding a
// dataset does; the readers keep running the hybrid mix. The measured thing is
// whether read latency degrades, so the result reports both halves.
func (r *Run) phaseMixed(queries []QuerySpec) {
	t := r.T
	hybrid := withKind(queries, "hybrid")
	readers := min(max(0, getenvInt("LT_READERS", 8)), len(r.clients))
	writers := min(max(0, getenvInt("LT_WRITERS", 2)), len(r.clients)-readers)
	dur := time.Duration(getenvInt("LT_MIXED_SECONDS", 20)) * time.Second
	if len(hybrid) == 0 {
		t.Log("P7 skipped: no queries")
		return
	}

	ctx, cancel := context.WithTimeout(context.Background(), dur)
	defer cancel()

	var readLats sync.Map // goroutine -> *[]time.Duration
	var readSuccess atomic.Int64
	var readErrs atomic.Int64
	var firstReadErrOnce sync.Once
	var firstReadErr error
	var aborted atomic.Bool // set once the server is gone; stops the phase
	var wg sync.WaitGroup

	// cancelOnServerLoss ends the phase when reads keep failing: at that point
	// the remaining seconds measure nothing, and a phase that "completes" with
	// millions of errors hides the real result.
	phaseCtx, stopPhase := context.WithCancel(ctx)
	defer stopPhase()
	go func() {
		tick := time.NewTicker(2 * time.Second)
		defer tick.Stop()
		last := int64(0)
		for {
			select {
			case <-phaseCtx.Done():
				return
			case <-tick.C:
				cur := readErrs.Load()
				if cur > 0 && cur == last {
					// no new errors this window; nothing to judge
					continue
				}
				last = cur
				if cur > 0 && readSuccess.Load() == 0 {
					aborted.Store(true)
					stopPhase()
					return
				}
			}
		}
	}()

	// Readers
	for i := 0; i < readers; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			eng := r.clients[id]
			lats := make([]time.Duration, 0, 512)
			j := id
			for phaseCtx.Err() == nil {
				s := hybrid[j%len(hybrid)]
				j++
				st := time.Now()
				if _, err := eng.RunQuery(phaseCtx, s); err != nil {
					// A cancelled context is the phase ending, not a failure;
					// the SDK surfaces it as an error like any other.
					if phaseCtx.Err() == nil {
						readErrs.Add(1)
						firstReadErrOnce.Do(func() { firstReadErr = err })
						// Back off on failure. Without this, a server that has
						// gone away turns the reader loop into a busy spin: one
						// run recorded 33,572,219 errors in 10 s and the error
						// count said more about the loop than about the server.
						select {
						case <-phaseCtx.Done():
						case <-time.After(50 * time.Millisecond):
						}
					}
					continue
				}
				readSuccess.Add(1)
				lats = append(lats, time.Since(st))
			}
			readLats.Store(id, lats)
		}(i)
	}

	// Writers: new tables, filled while the readers run. Each writer owns one
	// pooled client so concurrent CREATE TABLE statements exercise the server's
	// DDL path without sharing a Thrift connection.
	var written atomic.Int64
	var writerErrMu sync.Mutex
	var writerErrs []error
	recordWriterErr := func(err error) {
		writerErrMu.Lock()
		writerErrs = append(writerErrs, err)
		writerErrMu.Unlock()
	}
	// Per writer, not for the phase. A shared cap makes the writer count a
	// no-op: at `LT_WRITER_ROWS=400` a run with 2 writers and one with 6 both
	// stopped at ~400 rows total, so raising LT_WRITERS measured nothing. Each
	// writer now runs its own budget, so write pressure scales with the count.
	writerRows := getenvInt("LT_WRITER_ROWS", 2000)
	for i := 0; i < writers; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			eng := r.clients[readers+id]

			name := fmt.Sprintf("ragflow_lt_w%d_%d", id, time.Now().UnixNano()%100000)
			// P7 writer tables are always created without indexes, regardless of
			// LT_INDEX_LAST. Reason, measured: a CREATE INDEX issued on a table
			// that already has rows, while another connection is running SELECT,
			// segfaults the server ("Can't bind column expression ...
			// column_remapper_impl.cpp:148", then SIGSEGV). Building the index
			// with no rows present never crashed. So the mixed phase cannot
			// exercise the load-then-index order; that order is measured in P1
			// where nothing else is running.
			tbl, err := eng.CreateTable(name, false, r.Cfg.Sparsegram)
			if err != nil {
				recordWriterErr(fmt.Errorf("writer %d create %s: %w", id, name, err))
				return
			}
			defer eng.DB.DropTable(name, infinity.ConflictTypeIgnore)
			rng := rand.New(rand.NewSource(r.Cfg.Seed + int64(id)*104729))
			rb := &RowBuilder{Mapping: r.mapping, Dim: r.emb.Dim()}
			mine := 0
			for ctx.Err() == nil && mine < writerRows {
				rows := make([]Row, 0, 32)
				texts := make([]string, 0, 32)
				for k := 0; k < 32; k++ {
					art, text := r.corpus.Sample(rng)
					row, err := rb.Build(rng, art, text, fmt.Sprintf("%032x", rng.Int63()), "kb_w", k, 1)
					if err != nil {
						recordWriterErr(fmt.Errorf("writer %d build row: %w", id, err))
						return
					}
					rows = append(rows, row)
					texts = append(texts, text)
				}
				vecs, err := r.emb.Encode(texts, fallbackTokens)
				if err != nil {
					recordWriterErr(fmt.Errorf("writer %d encode: %w", id, err))
					return
				}
				if err := eng.IngestBatch(tbl, rows, vecs); err != nil {
					recordWriterErr(fmt.Errorf("writer %d insert: %w", id, err))
					return
				}
				written.Add(int64(len(rows)))
				mine += len(rows)
			}
		}(i)
	}
	wg.Wait()

	var all []time.Duration
	all = allReadLatencies(&readLats)
	p50, p99 := percentiles(all)
	if aborted.Load() {
		r.T.Errorf("P7: server stopped answering; phase aborted after %s", dur)
	} else if readers > 0 && len(all) == 0 {
		r.T.Errorf("P7: %d readers configured, but 0 reads succeeded (first error: %v)", readers, firstReadErr)
	}
	if writers > 0 && len(writerErrs) > 0 {
		r.T.Errorf("P7: %d writer error(s); first: %v", len(writerErrs), writerErrs[0])
	}
	detail := fmt.Sprintf("%d readers / %d writers for %s (%d fixed clients): %d reads, p50 %s p99 %s, %d read errors, %d rows written",
		readers, writers, dur, len(r.clients), len(all), p50.Round(time.Microsecond), p99.Round(time.Microsecond),
		readErrs.Load(), written.Load())
	if firstReadErr != nil {
		detail += fmt.Sprintf("; first error: %v", firstReadErr)
	}
	r.record("P7 mixed", dur, int(written.Load()), detail)
}

// phaseTeardown is P8: drop every table this run created.
func (r *Run) phaseTeardown() {
	if os.Getenv("LT_KEEP_TABLES") != "" {
		r.T.Log("P8 skipped: LT_KEEP_TABLES is set")
		r.teardownDone = true
		return
	}
	start := time.Now()
	dropped, dropErrs := r.dropTableList(r.alloc.Names)
	r.teardownDone = true
	if r.engine != nil && r.engine.Conn != nil && r.dbName != "" {
		if _, err := r.engine.Conn.DropDatabase(r.dbName, infinity.ConflictTypeIgnore); err != nil {
			dropErrs = append(dropErrs, fmt.Sprintf("drop database %s: %v", r.dbName, err))
		}
	}
	if len(dropErrs) > 0 {
		r.T.Errorf("P8 teardown failed (%d error(s)): %s", len(dropErrs), strings.Join(dropErrs, "; "))
	}
	r.record("P8 teardown", time.Since(start), 0, fmt.Sprintf("dropped %d tables", dropped))
}

func (r *Run) cleanupOnExit() {
	if r.teardownDone || os.Getenv("LT_KEEP_TABLES") != "" {
		return
	}
	r.createdMu.Lock()
	names := append([]string(nil), r.createdTables...)
	r.createdMu.Unlock()
	if len(names) > 0 {
		_, errs := r.dropTableList(names)
		if len(errs) > 0 {
			r.T.Errorf("cleanup teardown failed: %s", strings.Join(errs, "; "))
		}
	}
	if r.dbName != "" && r.engine != nil && r.engine.Conn != nil {
		if _, err := r.engine.Conn.DropDatabase(r.dbName, infinity.ConflictTypeIgnore); err != nil {
			r.T.Logf("cleanup drop database %s: %v", r.dbName, err)
		}
	}
}

func (r *Run) dropTableList(names []string) (int64, []string) {
	var dropped atomic.Int64
	var errMu sync.Mutex
	var errs []string
	addErr := func(msg string) {
		errMu.Lock()
		errs = append(errs, msg)
		errMu.Unlock()
	}
	if len(names) > 0 {
		if len(r.clients) == 0 {
			return 0, []string{"no Thrift clients"}
		}
		workers := min(len(r.clients), len(names))
		jobs := make(chan string, len(names))
		for _, name := range names {
			jobs <- name
		}
		close(jobs)
		var wg sync.WaitGroup
		for w := 0; w < workers; w++ {
			wg.Add(1)
			go func(worker int) {
				defer wg.Done()
				eng := r.clients[worker]
				for name := range jobs {
					if _, err := eng.DB.DropTable(name, infinity.ConflictTypeIgnore); err != nil {
						addErr(fmt.Sprintf("drop %s: %v", name, err))
						continue
					}
					dropped.Add(1)
				}
			}(w)
		}
		wg.Wait()
	}
	return dropped.Load(), errs
}

// printReport writes the phase table and the environment, so two runs can be
// compared without guessing what differed.
func (r *Run) printReport() {
	var b strings.Builder
	b.WriteString("\n=== load test report ===\n")
	fmt.Fprintf(&b, "scenario     %s (skew=%v)\n", r.Cfg.Scenario.Name, r.effectiveSkew)
	fmt.Fprintf(&b, "clients      %d (Go NumCPU=%d)\n", len(r.clients), runtime.NumCPU())
	fmt.Fprintf(&b, "database     %s\n", r.dbName)
	if r.mappingSHA != "" {
		fmt.Fprintf(&b, "mapping      sha256 %s\n", r.mappingSHA)
	}
	for _, spec := range PinnedModelFiles {
		if h, ok := r.modelHashes[spec.RelPath]; ok {
			fmt.Fprintf(&b, "model        %s sha256 %s\n", spec.RelPath, h)
		}
	}
	fmt.Fprintf(&b, "variant      %s (%d index objects/table)\n",
		variantName(r.Cfg.Sparsegram), r.mapping.IndexObjects(r.Cfg.Sparsegram))
	fmt.Fprintf(&b, "import order %s\n", map[bool]string{true: "load then index (P1b)", false: "indexes first (P1a)"}[r.Cfg.IndexLast])
	fmt.Fprintf(&b, "seed         %d\n", r.Cfg.Seed)
	fmt.Fprintf(&b, "tables/rows  %d / %d\n", len(r.alloc.Names), r.alloc.Total())
	fmt.Fprintf(&b, "go           %s, GOMAXPROCS=%d\n", runtime.Version(), runtime.GOMAXPROCS(0))
	b.WriteString("\nphase            seconds        rows  detail\n")
	var total float64
	for _, p := range r.report {
		fmt.Fprintf(&b, "%-16s %8.2f %11d  %s\n", p.Phase, p.Seconds, p.Rows, p.Detail)
		total += p.Seconds
	}
	fmt.Fprintf(&b, "%-16s %8.2f\n", "TOTAL", total)

	// Ingest cost attribution. The probes behind these numbers are in
	// tools/loadtest/README.md; they are repeated here because a reader looking
	// at a low rows/s figure needs to know where it came from, and the answer is
	// not "the vector index".
	b.WriteString("\ningest cost attribution (measured, 93-column table):\n")
	for _, line := range []string{
		"  no indexes                       ~3,300 rows/s",
		"  + HNSW (lvq) only                ~2,600 rows/s",
		"  + 11 secondary indexes only      ~3,400 rows/s",
		"  + full-text on whitespace/rank   ~3,300 rows/s",
		"  + ONE rag-coarse|rag-fine index     ~264 rows/s",
		"  + all 47 full-text (production)     ~230 rows/s",
		"",
		"  The first rag-* index costs ~12x and every further rag-* index is free:",
		"  the cost is per row in the RAG analyzer, not per index object. HNSW and",
		"  the secondary indexes are noise by comparison. So a plan that budgets",
		"  ingest from vector-index cost, or from index count, will be wrong by an",
		"  order of magnitude -- the 8.34M-row L scenario is ~10 hours of ingest at",
		"  this rate, not the ~1 hour the plan's 1,936 rows/s estimate implied.",
	} {
		b.WriteString(line + "\n")
	}
	r.T.Log(b.String())
}
