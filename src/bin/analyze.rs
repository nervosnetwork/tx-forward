use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
};

fn main() {
    let find_tx = regex::Regex::new("[a-z0-9]{64}").unwrap();
    let submit_success = regex::Regex::new("submit.*fork").unwrap();
    let submit_fail = regex::Regex::new("submit.*fail").unwrap();
    let commit_success = regex::Regex::new("committed success").unwrap();
    let commit_fail = regex::Regex::new("reject|null").unwrap();

    let mut submit_success_set = HashSet::new();
    let mut submit_fail_set = HashSet::new();
    let mut commit_success_set = HashSet::new();
    let mut commit_fail_set = HashSet::new();

    let fs = File::open("./data/logs/forward.log").unwrap();
    let reader = BufReader::new(fs);
    for line in reader.lines() {
        match line {
            Ok(l) => {
                let tx = match find_tx.captures(&l) {
                    Some(cap) => cap[0].to_owned(),
                    None => continue,
                };

                if submit_success.find(&l).is_some() {
                    submit_success_set.insert(tx);
                } else if submit_fail.find(&l).is_some() {
                    submit_fail_set.insert(tx);
                } else if commit_success.find(&l).is_some() {
                    commit_success_set.insert(tx);
                } else if commit_fail.find(&l).is_some() {
                    commit_fail_set.insert(tx);
                }
            }
            Err(_) => break,
        }
    }

    for i in commit_success_set
        .iter()
        .chain(commit_fail_set.iter())
        .chain(submit_success_set.iter())
    {
        submit_fail_set.remove(i);
    }
    for i in commit_success_set.iter() {
        commit_fail_set.remove(i);
    }
    println!(
        "submit success: {}, submit fail: {}, commit success: {}, commit fail: {}",
        submit_success_set.len(),
        submit_fail_set.len(),
        commit_success_set.len(),
        commit_fail_set.len()
    )
}
