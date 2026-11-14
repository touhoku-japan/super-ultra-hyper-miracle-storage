#!/bin/bash
# AWS CLI v2 での互換確認。稼働中の suhms に対して主要な操作を実行し、PASS/FAIL を表示する。
#
# 事前に設定する環境変数:
#   AWS_ENDPOINT_URL (例 http://127.0.0.1:9000), AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
#   AWS_DEFAULT_REGION（サーバの [s3] region）
# パス形式のアドレッシングが必要（AWS_CONFIG_FILE に `s3 =\n  addressing_style = path`）。
# バケット awscli-test / awscli-lock を作る（実行後も残る）。作業ファイルは一時ディレクトリに置く。
set -u
export AWS_PAGER=
W=$(mktemp -d); trap 'rm -rf "$W"' EXIT; mkdir -p "$W/up" "$W/down"; cd "$W"
B=awscli-test
pass=0; fail=0
t(){ local name=$1; shift; if out=$("$@" 2>&1); then echo "PASS $name"; pass=$((pass+1)); else echo "FAIL $name"; echo "$out" | tail -5 | sed 's/^/    /'; fail=$((fail+1)); fi; }
tf(){ local name=$1; shift; if out=$("$@" 2>&1); then echo "FAIL(expected error) $name"; fail=$((fail+1)); else echo "PASS $name -> $(echo "$out" | grep -o '([A-Za-z]*)' | head -1)"; pass=$((pass+1)); fi; }
head -c 1024 /dev/urandom > small.bin
head -c $((40*1024*1024)) /dev/urandom > big.bin   # multipart (default threshold 8MB)
for i in 1 2 3; do echo "file $i" > up/f$i.txt; done; mkdir -p up/sub; echo nested > up/sub/n.txt
# high-level s3
t "s3 mb"                 aws s3 mb s3://$B
t "s3 cp small"           aws s3 cp small.bin s3://$B/small.bin
t "s3 cp big multipart"   aws s3 cp big.bin s3://$B/big.bin
t "s3 cp download big"    aws s3 cp s3://$B/big.bin down/big.bin
t "big content equal"     cmp big.bin down/big.bin
t "s3 sync up"            aws s3 sync up s3://$B/dir/
t "s3 sync down"          aws s3 sync s3://$B/dir/ down/dir
t "sync content equal"    diff -r up down/dir
t "s3 sync no-op"         bash -c "[ -z \"\$(aws s3 sync up s3://$B/dir/)\" ]"
t "s3 ls recursive"       aws s3 ls s3://$B --recursive
t "s3 ls delimiter"       bash -c "aws s3 ls s3://$B/dir/ | grep -q 'PRE sub/'"
t "s3 cp server-side copy big" aws s3 cp s3://$B/big.bin s3://$B/big-copy.bin
t "s3 mv"                 aws s3 mv s3://$B/dir/f1.txt s3://$B/dir/f1-moved.txt
t "s3 rm recursive"       aws s3 rm s3://$B/dir/ --recursive
t "s3 cp stdin/stdout"    bash -c "echo hello | aws s3 cp - s3://$B/stdin.txt && [ \"\$(aws s3 cp s3://$B/stdin.txt -)\" = hello ]"
t "s3 presign + curl"     bash -c "curl -sf \"\$(aws s3 presign s3://$B/stdin.txt)\" | grep -q hello"
# s3api
t "head-object"           aws s3api head-object --bucket $B --key small.bin
t "get-object range"      bash -c "aws s3api get-object --bucket $B --key small.bin --range bytes=0-9 r.bin >/dev/null && [ \$(stat -c%s r.bin) = 10 ]"
t "head checksum-mode"    bash -c "aws s3api head-object --bucket $B --key small.bin --checksum-mode ENABLED | grep -q ChecksumCRC64NVME"
t "put sha256 checksum"   aws s3api put-object --bucket $B --key sha.bin --body small.bin --checksum-algorithm SHA256
t "get checksum validate" bash -c "aws s3api get-object --bucket $B --key sha.bin --checksum-mode ENABLED o.bin | grep -q ChecksumSHA256"
t "list-objects-v2"       aws s3api list-objects-v2 --bucket $B --max-keys 1
t "list-objects v1"       aws s3api list-objects --bucket $B
t "copy-object REPLACE metadata" aws s3api copy-object --bucket $B --key meta.bin --copy-source $B/small.bin --metadata-directive REPLACE --metadata a=b --content-type text/x-test
t "metadata kept"         bash -c "aws s3api head-object --bucket $B --key meta.bin | grep -q '\"a\": \"b\"'"
t "put-object-tagging"    aws s3api put-object-tagging --bucket $B --key small.bin --tagging 'TagSet=[{Key=env,Value=prod}]'
t "get-object-tagging"    bash -c "aws s3api get-object-tagging --bucket $B --key small.bin | grep -q prod"
t "put with --tagging"    aws s3api put-object --bucket $B --key tagged.bin --body small.bin --tagging 'a=1&b=2'
t "delete-object-tagging" aws s3api delete-object-tagging --bucket $B --key small.bin
tf "if-none-match * on existing" aws s3api put-object --bucket $B --key small.bin --body small.bin --if-none-match '*'
t "delete-objects"        aws s3api delete-objects --bucket $B --delete 'Objects=[{Key=sha.bin},{Key=meta.bin}]'
# versioning
t "put-bucket-versioning" aws s3api put-bucket-versioning --bucket $B --versioning-configuration Status=Enabled
t "get-bucket-versioning" bash -c "aws s3api get-bucket-versioning --bucket $B | grep -q Enabled"
echo v1 > v.txt; t "put v1" aws s3 cp v.txt s3://$B/v.txt
echo v2 > v.txt; t "put v2" aws s3 cp v.txt s3://$B/v.txt
t "list-object-versions"  bash -c "[ \$(aws s3api list-object-versions --bucket $B --prefix v.txt --query 'length(Versions)') = 2 ]"
V1=$(aws s3api list-object-versions --bucket $B --prefix v.txt --query 'Versions[?IsLatest==`false`].VersionId' --output text)
t "get old version"       bash -c "aws s3api get-object --bucket $B --key v.txt --version-id $V1 old.txt >/dev/null && grep -q v1 old.txt"
t "copy from version"     aws s3api copy-object --bucket $B --key v-restored.txt --copy-source "$B/v.txt?versionId=$V1"
t "delete -> marker"      aws s3 rm s3://$B/v.txt
t "versions has marker"   bash -c "aws s3api list-object-versions --bucket $B --prefix v.txt | grep -q DeleteMarkers"
# multipart via s3api
UP=$(aws s3api create-multipart-upload --bucket $B --key mpu.bin --query UploadId --output text)
head -c $((5*1024*1024)) /dev/urandom > p1; head -c 1000 /dev/urandom > p2
E1=$(aws s3api upload-part --bucket $B --key mpu.bin --upload-id $UP --part-number 1 --body p1 --query ETag --output text)
E2=$(aws s3api upload-part --bucket $B --key mpu.bin --upload-id $UP --part-number 2 --body p2 --query ETag --output text)
t "list-multipart-uploads" bash -c "aws s3api list-multipart-uploads --bucket $B | grep -q $UP"
t "list-parts"            bash -c "[ \$(aws s3api list-parts --bucket $B --key mpu.bin --upload-id $UP --query 'length(Parts)') = 2 ]"
t "complete-multipart"    aws s3api complete-multipart-upload --bucket $B --key mpu.bin --upload-id $UP --multipart-upload "Parts=[{PartNumber=1,ETag=$E1},{PartNumber=2,ETag=$E2}]"
t "mpu content"           bash -c "aws s3 cp s3://$B/mpu.bin m.bin >/dev/null && cat p1 p2 | cmp - m.bin"
UP2=$(aws s3api create-multipart-upload --bucket $B --key abort.bin --query UploadId --output text)
t "abort-multipart"       aws s3api abort-multipart-upload --bucket $B --key abort.bin --upload-id $UP2
UP3=$(aws s3api create-multipart-upload --bucket $B --key upc.bin --query UploadId --output text)
t "upload-part-copy"      aws s3api upload-part-copy --bucket $B --key upc.bin --upload-id $UP3 --part-number 1 --copy-source $B/big.bin --copy-source-range bytes=0-5242879
aws s3api abort-multipart-upload --bucket $B --key upc.bin --upload-id $UP3 >/dev/null 2>&1
# bucket configs
t "put-bucket-lifecycle"  aws s3api put-bucket-lifecycle-configuration --bucket $B --lifecycle-configuration '{"Rules":[{"ID":"r1","Status":"Enabled","Filter":{"And":{"Prefix":"logs/","Tags":[{"Key":"t","Value":"1"}]}},"Expiration":{"Days":30},"NoncurrentVersionExpiration":{"NoncurrentDays":7},"Transitions":[{"Days":10,"StorageClass":"STANDARD_IA"}]}]}'
t "get-bucket-lifecycle"  bash -c "aws s3api get-bucket-lifecycle-configuration --bucket $B | grep -q STANDARD_IA"
t "delete-bucket-lifecycle" aws s3api delete-bucket-lifecycle --bucket $B
t "put-bucket-cors"       aws s3api put-bucket-cors --bucket $B --cors-configuration '{"CORSRules":[{"AllowedMethods":["GET"],"AllowedOrigins":["*"]}]}'
t "get-bucket-cors"       aws s3api get-bucket-cors --bucket $B
t "delete-bucket-cors"    aws s3api delete-bucket-cors --bucket $B
t "get-bucket-location"   aws s3api get-bucket-location --bucket $B
t "storage-class REDUCED_REDUNDANCY" bash -c "aws s3 cp small.bin s3://$B/rr.bin --storage-class REDUCED_REDUNDANCY && aws s3api head-object --bucket $B --key rr.bin | grep -q REDUCED_REDUNDANCY"
tf "storage-class unknown" aws s3 cp small.bin s3://$B/x.bin --storage-class GLACIER
# object lock bucket
t "mb with object lock"   aws s3api create-bucket --bucket awscli-lock --object-lock-enabled-for-bucket
t "put-object-lock-config" aws s3api put-object-lock-configuration --bucket awscli-lock --object-lock-configuration '{"ObjectLockEnabled":"Enabled","Rule":{"DefaultRetention":{"Mode":"GOVERNANCE","Days":1}}}'
t "put locked object"     aws s3 cp small.bin s3://awscli-lock/l.bin
LV=$(aws s3api head-object --bucket awscli-lock --key l.bin --query VersionId --output text)
t "get-object-retention"  aws s3api get-object-retention --bucket awscli-lock --key l.bin
tf "delete locked version" aws s3api delete-object --bucket awscli-lock --key l.bin --version-id $LV
t "bypass governance"     aws s3api delete-object --bucket awscli-lock --key l.bin --version-id $LV --bypass-governance-retention
t "put-object-legal-hold" bash -c "aws s3 cp small.bin s3://awscli-lock/h.bin && aws s3api put-object-legal-hold --bucket awscli-lock --key h.bin --legal-hold Status=ON && aws s3api get-object-legal-hold --bucket awscli-lock --key h.bin | grep -q ON"
tf "bucket not empty"     aws s3api delete-bucket --bucket awscli-lock
tf "no such key"          aws s3api head-object --bucket $B --key nope
tf "no such bucket"       aws s3 ls s3://no-such-bucket-xyz
tf "reserved bucket suhms" aws s3 mb s3://suhms
echo "TOTAL pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
