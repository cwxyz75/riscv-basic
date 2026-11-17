/* This is a generated file, don't edit */

#define NUM_APPLETS 87
#define KNOWN_APPNAME_OFFSETS 4

const uint16_t applet_nameofs[] ALIGN2 = {
99,
225,
354,
};

const char applet_names[] ALIGN1 = ""
"[" "\0"
"[[" "\0"
"awk" "\0"
"base64" "\0"
"basename" "\0"
"cat" "\0"
"chmod" "\0"
"cksum" "\0"
"cmp" "\0"
"comm" "\0"
"cp" "\0"
"cut" "\0"
"date" "\0"
"dd" "\0"
"df" "\0"
"diff" "\0"
"dirname" "\0"
"du" "\0"
"echo" "\0"
"egrep" "\0"
"env" "\0"
"expand" "\0"
"expr" "\0"
"factor" "\0"
"false" "\0"
"fgrep" "\0"
"find" "\0"
"fold" "\0"
"grep" "\0"
"groups" "\0"
"head" "\0"
"hexdump" "\0"
"id" "\0"
"install" "\0"
"link" "\0"
"ln" "\0"
"logname" "\0"
"ls" "\0"
"md5sum" "\0"
"mkdir" "\0"
"mkfifo" "\0"
"mktemp" "\0"
"mv" "\0"
"nl" "\0"
"nohup" "\0"
"nproc" "\0"
"od" "\0"
"paste" "\0"
"printf" "\0"
"pwd" "\0"
"readlink" "\0"
"realpath" "\0"
"rev" "\0"
"rm" "\0"
"rmdir" "\0"
"sed" "\0"
"seq" "\0"
"sha1sum" "\0"
"sha256sum" "\0"
"sha512sum" "\0"
"shuf" "\0"
"sleep" "\0"
"sort" "\0"
"split" "\0"
"stat" "\0"
"sum" "\0"
"tac" "\0"
"tail" "\0"
"tee" "\0"
"test" "\0"
"time" "\0"
"timeout" "\0"
"touch" "\0"
"tr" "\0"
"true" "\0"
"truncate" "\0"
"tsort" "\0"
"uname" "\0"
"unexpand" "\0"
"uniq" "\0"
"unlink" "\0"
"wc" "\0"
"which" "\0"
"whoami" "\0"
"xargs" "\0"
"xxd" "\0"
"yes" "\0"
;

#define APPLET_NO_awk 2
#define APPLET_NO_base64 3
#define APPLET_NO_basename 4
#define APPLET_NO_cat 5
#define APPLET_NO_chmod 6
#define APPLET_NO_cksum 7
#define APPLET_NO_cmp 8
#define APPLET_NO_comm 9
#define APPLET_NO_cp 10
#define APPLET_NO_cut 11
#define APPLET_NO_date 12
#define APPLET_NO_dd 13
#define APPLET_NO_df 14
#define APPLET_NO_diff 15
#define APPLET_NO_dirname 16
#define APPLET_NO_du 17
#define APPLET_NO_echo 18
#define APPLET_NO_egrep 19
#define APPLET_NO_env 20
#define APPLET_NO_expand 21
#define APPLET_NO_expr 22
#define APPLET_NO_factor 23
#define APPLET_NO_false 24
#define APPLET_NO_fgrep 25
#define APPLET_NO_find 26
#define APPLET_NO_fold 27
#define APPLET_NO_grep 28
#define APPLET_NO_groups 29
#define APPLET_NO_head 30
#define APPLET_NO_hexdump 31
#define APPLET_NO_id 32
#define APPLET_NO_install 33
#define APPLET_NO_link 34
#define APPLET_NO_ln 35
#define APPLET_NO_logname 36
#define APPLET_NO_ls 37
#define APPLET_NO_md5sum 38
#define APPLET_NO_mkdir 39
#define APPLET_NO_mkfifo 40
#define APPLET_NO_mktemp 41
#define APPLET_NO_mv 42
#define APPLET_NO_nl 43
#define APPLET_NO_nohup 44
#define APPLET_NO_nproc 45
#define APPLET_NO_od 46
#define APPLET_NO_paste 47
#define APPLET_NO_printf 48
#define APPLET_NO_pwd 49
#define APPLET_NO_readlink 50
#define APPLET_NO_realpath 51
#define APPLET_NO_rev 52
#define APPLET_NO_rm 53
#define APPLET_NO_rmdir 54
#define APPLET_NO_sed 55
#define APPLET_NO_seq 56
#define APPLET_NO_sha1sum 57
#define APPLET_NO_sha256sum 58
#define APPLET_NO_sha512sum 59
#define APPLET_NO_shuf 60
#define APPLET_NO_sleep 61
#define APPLET_NO_sort 62
#define APPLET_NO_split 63
#define APPLET_NO_stat 64
#define APPLET_NO_sum 65
#define APPLET_NO_tac 66
#define APPLET_NO_tail 67
#define APPLET_NO_tee 68
#define APPLET_NO_test 69
#define APPLET_NO_time 70
#define APPLET_NO_timeout 71
#define APPLET_NO_touch 72
#define APPLET_NO_tr 73
#define APPLET_NO_true 74
#define APPLET_NO_truncate 75
#define APPLET_NO_tsort 76
#define APPLET_NO_uname 77
#define APPLET_NO_unexpand 78
#define APPLET_NO_uniq 79
#define APPLET_NO_unlink 80
#define APPLET_NO_wc 81
#define APPLET_NO_which 82
#define APPLET_NO_whoami 83
#define APPLET_NO_xargs 84
#define APPLET_NO_xxd 85
#define APPLET_NO_yes 86

#ifndef SKIP_applet_main
int (*const applet_main[])(int argc, char **argv) = {
test_main,
test_main,
awk_main,
baseNUM_main,
basename_main,
cat_main,
chmod_main,
cksum_main,
cmp_main,
comm_main,
cp_main,
cut_main,
date_main,
dd_main,
df_main,
diff_main,
dirname_main,
du_main,
echo_main,
grep_main,
env_main,
expand_main,
expr_main,
factor_main,
false_main,
grep_main,
find_main,
fold_main,
grep_main,
id_main,
head_main,
hexdump_main,
id_main,
install_main,
link_main,
ln_main,
logname_main,
ls_main,
md5_sha1_sum_main,
mkdir_main,
mkfifo_main,
mktemp_main,
mv_main,
nl_main,
nohup_main,
nproc_main,
od_main,
paste_main,
printf_main,
pwd_main,
readlink_main,
realpath_main,
rev_main,
rm_main,
rmdir_main,
sed_main,
seq_main,
md5_sha1_sum_main,
md5_sha1_sum_main,
md5_sha1_sum_main,
shuf_main,
sleep_main,
sort_main,
split_main,
stat_main,
sum_main,
tac_main,
tail_main,
tee_main,
test_main,
time_main,
timeout_main,
touch_main,
tr_main,
true_main,
truncate_main,
tsort_main,
uname_main,
expand_main,
uniq_main,
unlink_main,
wc_main,
which_main,
whoami_main,
xargs_main,
xxd_main,
yes_main,
};
#endif

const uint8_t applet_flags[] ALIGN1 = {
0x2f,
0xa3,
0xa0,
0x2a,
0x33,
0x22,
0xa3,
0xa8,
0xb2,
0xeb,
0x2a,
0x8e,
0xff,
0x38,
0xaa,
0x22,
0x22,
0x0c,
0xf3,
0x0c,
0xf3,
0x2a,
};

