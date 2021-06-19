#![cfg(all(any(target_arch = "x86", target_arch = "x86_64"), target_feature = "avx2"))]

use parasailors::{Matrix, *};

use block_aligner::scan_block::*;
use block_aligner::scores::*;

use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};

fn test(file_name: &str, verbose: bool) -> (usize, f64, usize) {
    let mut wrong = 0usize;
    let mut wrong_avg = 0f64;
    let mut count = 0usize;
    let reader = BufReader::new(File::open(file_name).unwrap());
    let all_lines = reader.lines().collect::<Vec<_>>();

    for lines in all_lines.chunks(2) {
        let r = lines[0].as_ref().unwrap().to_ascii_uppercase();
        let q = lines[1].as_ref().unwrap().to_ascii_uppercase();

        // parasail
        let matrix = Matrix::new(MatrixType::IdentityWithPenalty);
        let profile = Profile::new(q.as_bytes(), &matrix);
        let parasail_score = global_alignment_score(&profile, r.as_bytes(), 2, 1);

        let r_padded = PaddedBytes::from_bytes(r.as_bytes(), 2048, &NW1);
        let q_padded = PaddedBytes::from_bytes(q.as_bytes(), 2048, &NW1);
        let run_gaps = Gaps { open: -2, extend: -1 };

        // ours
        let block_aligner = Block::<_, false, false>::align(&q_padded, &r_padded, &NW1, run_gaps, 32..=2048, 0);
        let scan_score = block_aligner.res().score;

        if parasail_score != scan_score {
            wrong += 1;
            wrong_avg += ((parasail_score - scan_score) as f64) / (parasail_score as f64);

            if verbose {
                println!(
                    "parasail: {}, ours: {}\nq (len = {}): {}\nr (len = {}): {}",
                    parasail_score,
                    scan_score,
                    q.len(),
                    q,
                    r.len(),
                    r
                );
            }
        }

        count += 1;
    }

    (wrong, wrong_avg / (wrong as f64), count)
}

fn main() {
    let arg1 = env::args().skip(1).next();
    let verbose = arg1.is_some() && arg1.unwrap() == "-v";
    let (wrong, wrong_avg, count) = test("data/supplementary_data/sequences.txt", verbose);
    println!("\ntotal: {}, wrong: {}, wrong avg: {}", count, wrong, wrong_avg);
    println!("Done!");
}
