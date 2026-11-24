// DuckDB multi-threading stress test.
//
// Several client threads share one file-backed database whose task scheduler
// runs its own worker threads. The clients concurrently append, insert in
// transactions that commit or roll back, update one hot row (write-write
// conflicts), checkpoint, run parallel aggregations, joins, and sorts, and
// interrupt a running query. Every result whose value is determined is
// checked, so a wrong interleaving shows up as a failure, not only as a hang.
//
// Usage: duckdb_stress [scale] [db_threads]
//
// Environment, for trying DuckDB configurations that change how it uses
// threads:
//   DUCKDB_SETTINGS  comma-separated name=value database options, such as
//                    "external_threads=2,scheduler_process_partial=true"
//   DUCKDB_PRAGMAS   semicolon-separated SQL run on every connection, such
//                    as "PRAGMA verify_parallelism"
//   DUCKDB_PATH      ":memory:" for an in-memory database instead of a file
#include "duckdb.hpp"

#include <algorithm>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <string>
#include <thread>
#include <ftw.h>
#include <unistd.h>
#include <vector>

using namespace duckdb;

namespace {

std::atomic<int> failures {0};

void Fail(const std::string &what) {
	failures.fetch_add(1);
	std::fprintf(stderr, "FAIL: %s\n", what.c_str());
}

// Runs a query that must succeed and returns its materialized result.
unique_ptr<MaterializedQueryResult> Must(Connection &con, const std::string &sql) {
	auto result = con.Query(sql);
	if (result->HasError()) {
		Fail(sql + ": " + result->GetError());
	}
	return result;
}

// Splits `text` at `sep`, dropping empty parts.
std::vector<std::string> Split(const std::string &text, char sep) {
	std::vector<std::string> parts;
	size_t start = 0;
	while (start <= text.size()) {
		size_t end = text.find(sep, start);
		if (end == std::string::npos) {
			end = text.size();
		}
		if (end > start) {
			parts.push_back(text.substr(start, end - start));
		}
		start = end + 1;
	}
	return parts;
}

std::string Env(const char *name) {
	const char *value = std::getenv(name);
	return value ? value : "";
}

// Runs DUCKDB_PRAGMAS on a new connection.
void Setup(Connection &con) {
	for (auto &sql : Split(Env("DUCKDB_PRAGMAS"), ';')) {
		Must(con, sql);
	}
}

int64_t Scalar(Connection &con, const std::string &sql) {
	auto result = Must(con, sql);
	if (result->HasError() || result->RowCount() != 1) {
		return INT64_MIN;
	}
	return result->GetValue(0, 0).GetValue<int64_t>();
}

void Expect(Connection &con, const std::string &sql, int64_t expected) {
	auto actual = Scalar(con, sql);
	if (actual != expected) {
		Fail(sql + ": expected " + std::to_string(expected) + ", got " + std::to_string(actual));
	}
}

} // namespace

int main(int argc, char **argv) {
	// Scale multiplies every size; fractions shrink the run for rsched, which
	// makes each atomic operation a scheduling point.
	const double scale = argc > 1 ? std::atof(argv[1]) : 1;
	const int64_t db_threads = argc > 2 ? std::atoll(argv[2]) : 4;
	auto scaled = [&](int64_t base) { return std::max<int64_t>(1, int64_t(base * scale)); };
	const int64_t appenders = 2, rows_per_appender = scaled(20000), batch = scaled(1000);
	const int64_t inserters = 2, txns_per_inserter = scaled(10), rows_per_txn = scaled(500);
	const int64_t updaters = 3, updates_per_updater = scaled(20);
	const int64_t readers = 2, queries_per_reader = scaled(3);
	// Multiples of 1000 keep the GROUP BY and join answers exact.
	const int64_t range_n = scaled(200) * 1000;

	char dir_template[] = "/tmp/duckdb-stress-XXXXXX";
	if (!mkdtemp(dir_template)) {
		std::perror("mkdtemp");
		return 2;
	}
	const std::string path =
	    Env("DUCKDB_PATH") == ":memory:" ? std::string(":memory:") : std::string(dir_template) + "/stress.db";

	DBConfig config;
	config.options.maximum_threads = db_threads;
	config.SetOptionByName("checkpoint_threshold", Value("256KB"));
	config.SetOptionByName("temp_directory", Value(std::string(dir_template) + "/tmp"));
	for (auto &setting : Split(Env("DUCKDB_SETTINGS"), ',')) {
		auto eq = setting.find('=');
		if (eq == std::string::npos) {
			std::fprintf(stderr, "DUCKDB_SETTINGS: expected name=value, got %s\n", setting.c_str());
			return 2;
		}
		config.SetOptionByName(setting.substr(0, eq), Value(setting.substr(eq + 1)));
	}
	DuckDB db(path, &config);
	{
		Connection con(db);
		Setup(con);
		Must(con, "CREATE TABLE t (id BIGINT, k INTEGER, v BIGINT)");
		Must(con, "CREATE TABLE hot (id INTEGER PRIMARY KEY, n BIGINT)");
		Must(con, "INSERT INTO hot VALUES (1, 0)");
	}

	std::atomic<int64_t> committed_rows {0}, committed_sum {0}, hot_commits {0}, conflicts {0};
	std::atomic<int64_t> checkpoints {0}, interrupts {0};
	std::atomic<bool> writers_done {false}, interrupted {false};
	std::vector<std::thread> threads;

	// Appender ids are disjoint per thread; v = id, so sums are known.
	for (int64_t a = 0; a < appenders; a++) {
		threads.emplace_back([&, a] {
			Connection con(db);
			Setup(con);
			Appender appender(con, "t");
			for (int64_t i = 0; i < rows_per_appender; i++) {
				int64_t id = a * rows_per_appender + i;
				appender.AppendRow(id, int32_t(id % 97), id);
				if ((i + 1) % batch == 0) {
					appender.Flush();
				}
			}
			appender.Close();
			committed_rows += rows_per_appender;
			int64_t lo = a * rows_per_appender, hi = lo + rows_per_appender - 1;
			committed_sum += (lo + hi) * rows_per_appender / 2;
		});
	}

	// Transactional INSERT ... SELECT; every third transaction rolls back.
	for (int64_t w = 0; w < inserters; w++) {
		threads.emplace_back([&, w] {
			Connection con(db);
			Setup(con);
			for (int64_t txn = 0; txn < txns_per_inserter; txn++) {
				Must(con, "BEGIN TRANSACTION");
				Must(con, "INSERT INTO t SELECT -1, (i % 97)::INTEGER, 1 FROM range(" +
				              std::to_string(rows_per_txn) + ") r(i)");
				if ((txn + w) % 3 == 0) {
					Must(con, "ROLLBACK");
				} else {
					Must(con, "COMMIT");
					committed_rows += rows_per_txn;
					committed_sum += rows_per_txn;
				}
			}
		});
	}

	// Every updater increments the same row, so transactions conflict and
	// must retry; the final value counts the commits that succeeded.
	for (int64_t u = 0; u < updaters; u++) {
		threads.emplace_back([&] {
			Connection con(db);
			Setup(con);
			for (int64_t i = 0; i < updates_per_updater;) {
				auto result = con.Query("UPDATE hot SET n = n + 1 WHERE id = 1");
				if (result->HasError()) {
					conflicts++;
					std::this_thread::yield();
					continue;
				}
				hot_commits++;
				i++;
			}
		});
	}

	// Checkpoints race with writers; "other write transactions active" is fine.
	threads.emplace_back([&] {
		Connection con(db);
		Setup(con);
		while (!writers_done.load()) {
			auto result = con.Query("CHECKPOINT");
			if (!result->HasError()) {
				checkpoints++;
			}
			std::this_thread::yield();
		}
	});

	// Parallel scans, hash aggregates, hash joins, sorts, and window functions
	// with known answers, plus a monotonic check on the table being written.
	for (int64_t r = 0; r < readers; r++) {
		threads.emplace_back([&] {
			Connection con(db);
			Setup(con);
			const std::string n = std::to_string(range_n);
			int64_t last_count = 0;
			for (int64_t q = 0; q < queries_per_reader; q++) {
				Expect(con, "SELECT sum(i) FROM range(" + n + ") r(i)", range_n * (range_n - 1) / 2);
				Expect(con,
				       "SELECT count(*) FROM (SELECT i % 1000 g, count(*) c FROM range(" + n +
				           ") r(i) GROUP BY g) WHERE c = " + std::to_string(range_n / 1000),
				       1000);
				Expect(con,
				       "SELECT count(*) FROM range(" + n + ") a(i) JOIN range(" + n +
				           ") b(j) ON a.i = b.j * 2",
				       range_n / 2);
				Expect(con,
				       "SELECT count(*) FROM (SELECT i, row_number() OVER (ORDER BY i DESC) rn FROM range(" + n +
				           ") r(i)) WHERE i + rn = " + n,
				       range_n);
				int64_t count = Scalar(con, "SELECT count(*) FROM t");
				if (count < last_count) {
					Fail("SELECT count(*) FROM t decreased from " + std::to_string(last_count) + " to " +
					     std::to_string(count));
				}
				last_count = count;
			}
		});
	}

	// One connection runs a long query while another thread interrupts it.
	Connection victim(db);
	Setup(victim);
	threads.emplace_back([&] {
		for (int attempt = 0; attempt < 20 && !interrupted.load(); attempt++) {
			auto result = victim.Query("SELECT count(*) FROM range(" + std::to_string(range_n * 50) +
			                           ") a(i), range(10) b(j) WHERE (i * j) % 7 = 3");
			if (result->HasError() && result->GetError().find("Interrupt") != std::string::npos) {
				interrupted = true;
			}
		}
		interrupted = true;
	});
	threads.emplace_back([&] {
		while (!interrupted.load()) {
			victim.Interrupt();
			interrupts++;
			std::this_thread::yield();
		}
	});

	// Writers are the first appenders + inserters + updaters threads.
	const size_t writer_count = size_t(appenders + inserters + updaters);
	for (size_t i = 0; i < writer_count; i++) {
		threads[i].join();
	}
	writers_done = true;
	for (size_t i = writer_count; i < threads.size(); i++) {
		threads[i].join();
	}

	{
		Connection con(db);
		Setup(con);
		Expect(con, "SELECT count(*) FROM t", committed_rows.load());
		Expect(con, "SELECT sum(v) FROM t", committed_sum.load());
		Expect(con, "SELECT n FROM hot WHERE id = 1", hot_commits.load());
		Expect(con, "SELECT count(DISTINCT id) FROM t WHERE id >= 0", appenders * rows_per_appender);
		Must(con, "CHECKPOINT");
	}

	std::printf("rows=%lld sum=%lld hot=%lld conflicts=%lld checkpoints=%lld interrupts=%lld failures=%d\n",
	            (long long)committed_rows.load(), (long long)committed_sum.load(), (long long)hot_commits.load(),
	            (long long)conflicts.load(), (long long)checkpoints.load(), (long long)interrupts.load(),
	            failures.load());
	// Remove the database in-process: a shell would inherit the instrumented
	// libc through LD_PRELOAD under rsched.
	nftw(dir_template, [](const char *file, const struct stat *, int, struct FTW *) { return remove(file); }, 8,
	     FTW_DEPTH | FTW_PHYS);
	return failures.load() == 0 ? 0 : 1;
}
