#!/bin/sh
# Summarises a record-store file (spectrawl.db or a trawl_db run) for the
# classification review: labels per tuning with their window SNR, emitters
# whose records disagree, and verdicts with their evidence.
#
#     docs/review/classify_report.sh <db> [session]
db=$1; s=${2:-1}
q() { sqlite3 -header -column "$db" "$1"; echo; }
echo "## classifications per tuning and label (session $s)"
q "select round(tuner_hz/1e6,1) stop, label, count(*) n, round(avg(snr_db),1) snr_db, round(avg(bandwidth_hz)/1e3,1) bw_khz, round(avg(envelope_cv),2) cv, round(avg(analysed_s),2) secs from classifications where session=$s group by 1,2 having n>=3 order by 1, n desc;"
echo "## feature labels given at low window SNR (should be none below the weak gate)"
q "select label, count(*) n, round(avg(snr_db),1) snr from classifications where session=$s and protocol is null and snr_db < 7 and label not in ('Weak','too short','IQ expired','interrupted') group by 1 order by n desc;"
echo "## emitters with more than one feature label (disagreement)"
q "select serial, emitter_id, round(center_hz/1e6,4) mhz, round(bandwidth_hz/1e3,1) bw_khz, records, label, tally from emitters where session=$s and confirmed=1 and tally like '%,%' order by records desc limit 40;"
echo "## verdicts and their evidence"
q "select datetime(time,'unixepoch','localtime') t, receiver rx, round(center_hz/1e6,4) mhz, round(bandwidth_hz/1e3,1) bw_khz, kind, label, ident, in_place, substr(evidence,1,90) evidence from verdicts where session=$s order by time desc limit 60;"
echo "## classifier time per label"
q "select label, count(*) n, round(avg(ms)) ms, round(sum(ms)/1000,1) total_s from classifications where session=$s group by 1 order by total_s desc limit 15;"
