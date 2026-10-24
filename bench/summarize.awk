# Joins benchmark output (one file per engine, any number of rounds in each)
# into one table: each engine's best time per benchmark, and the winner with
# its margin over the runner-up. Each file's first word names its engine.
#
#   awk -f summarize.awk spirodb.txt sqlite.txt postgres.txt mariadb.txt

FNR == 1 {
    file++
    label[file] = $1
}

/ µs\/op/ {
    line = $0
    gsub(/×/, "x", line) # spiro-bench writes "readrandom ×10", the C benchmarks "x10"
    head = substr(line, 1, index(line, " µs/op") - 1)
    value = head
    sub(/.* /, "", value)
    name = head
    sub(/ +[0-9.]+$/, "", name)
    if (!(name in seen)) {
        seen[name] = 1
        order[++rows] = name
    }
    if (!((file, name) in best) || value + 0 < best[file, name]) best[file, name] = value + 0
}

END {
    printf "%-24s", "benchmark"
    for (f = 1; f <= file; f++) printf "%12s", label[f]
    printf "   fastest\n"
    for (i = 1; i <= rows; i++) {
        name = order[i]
        winner = 0
        printf "%-24s", name
        for (f = 1; f <= file; f++) {
            if ((f, name) in best) {
                printf "%12.3f", best[f, name]
                if (!winner || best[f, name] < best[winner, name]) winner = f
            } else {
                printf "%12s", "-"
            }
        }
        runner_up = 0
        for (f = 1; f <= file; f++)
            if (f != winner && (f, name) in best && (!runner_up || best[f, name] < best[runner_up, name])) runner_up = f
        if (runner_up) printf "   %s, %.1fx\n", label[winner], best[runner_up, name] / best[winner, name]
        else printf "   %s\n", label[winner]
    }
}
