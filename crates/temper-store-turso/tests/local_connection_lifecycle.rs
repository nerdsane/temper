//! Regression coverage for local libSQL connection teardown.

use std::sync::Barrier;

const WORKER_COUNT: usize = 8;
const LIFECYCLES_PER_WORKER: usize = 128;

#[test]
fn local_connections_survive_parallel_runtime_teardown() {
    let start = Barrier::new(WORKER_COUNT);

    std::thread::scope(|scope| {
        let workers = (0..WORKER_COUNT)
            .map(|_| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..LIFECYCLES_PER_WORKER {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("build isolated current-thread runtime");
                        runtime.block_on(async {
                            let database = libsql::Builder::new_local(":memory:")
                                .build()
                                .await
                                .expect("build local database");
                            let connection = database.connect().expect("open local connection");
                            connection
                                .execute("CREATE TABLE lifecycle_probe (value INTEGER)", ())
                                .await
                                .expect("execute on local connection");
                        });
                    }
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().expect("local database lifecycle worker");
        }
    });
}
