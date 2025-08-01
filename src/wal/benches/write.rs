// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use tempfile::tempdir;
use wal::{Writer, build_file_path};

pub fn write_benchmark(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let dir = dir.path();
    let mut group = c.benchmark_group("wal/write");
    for buf_size in [4096, 8192, 16384, 32768] {
        for entry_size in [4096, 16384, 32768, 65536] {
            let (mut writer, _) = Writer::new(
                build_file_path(dir, "org", "stream", entry_size.to_string()),
                1024_1024,
                buf_size,
                None,
            )
            .unwrap();
            let data = vec![42u8; entry_size];
            group.bench_function(BenchmarkId::new(buf_size.to_string(), entry_size), |b| {
                b.iter(|| {
                    writer.write(black_box(&data)).unwrap();
                    writer.sync().unwrap();
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, write_benchmark);
criterion_main!(benches);
