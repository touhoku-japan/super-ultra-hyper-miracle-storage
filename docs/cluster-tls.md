# ノード間通信の相互 TLS

各ノードの `[cluster] listen`（ノード間 API）を相互 TLS（mTLS）で保護する手順です。

- クラスタ専用のプライベート CA を 1 つ作り、各ノードの証明書をそれで署名します
- ノード間 API は、この CA が署名したクライアント証明書を出さない接続を TLS ハンドシェイクで拒否します
- ノードは相手のサーバ証明書もこの CA でだけ検証します（OS の信頼ストアは使いません）
- 従来の HMAC 認証（`[cluster] secret_file`）はそのまま残り、二重のチェックになります

## 1. CA を作る（1 回だけ、安全な端末で）

```sh
openssl req -x509 -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout cluster-ca.key -out cluster-ca.pem -days 3650 -subj "/CN=suhms cluster CA"
```

`cluster-ca.key` はノードに置かず、オフラインで保管してください（証明書の追加・更新のときだけ使う）。

## 2. ノードごとの証明書を作る

SAN（subjectAltName）には、他のノードが `[cluster] nodes` の `url` で使うホスト名か IP を必ず入れます。
同じ証明書をサーバとクライアントの両方に使うので、extendedKeyUsage に serverAuth と clientAuth を入れます。

```sh
NODE=node1
IP=10.0.0.1
openssl req -new -nodes -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout $NODE.key -out $NODE.csr -subj "/CN=$NODE"
openssl x509 -req -in $NODE.csr -CA cluster-ca.pem -CAkey cluster-ca.key -CAcreateserial \
  -out $NODE.pem -days 825 \
  -extfile <(printf "subjectAltName=IP:$IP\nextendedKeyUsage=serverAuth,clientAuth\nbasicConstraints=CA:FALSE\n")
```

## 3. 配置と設定

各ノードに `cluster-ca.pem`、自分の `<node>.pem` と `<node>.key` を置きます（鍵は `suhms` ユーザーだけが読めるように）。

```sh
install -d -o root -g suhms -m 0750 /etc/suhms/tls
install -o root -g suhms -m 0640 cluster-ca.pem node1.pem node1.key /etc/suhms/tls/
```

```toml
[cluster]
listen = "10.0.0.1:9100"
secret_file = "/etc/suhms/cluster.key"
nodes = [
  { id = "node1", url = "https://10.0.0.1:9100" },
  { id = "node2", url = "https://10.0.0.2:9100" },
  { id = "node3", url = "https://10.0.0.3:9100" },
]

[cluster.tls]
cert_file = "/etc/suhms/tls/node1.pem"
key_file = "/etc/suhms/tls/node1.key"
ca_file = "/etc/suhms/tls/cluster-ca.pem"
```

`[cluster.tls]` があるのに `http://` の url がある（またはその逆）と、起動時にエラーになります。

## 4. 切り替え

TLS と平文は混在できないので、全ノードを同時に切り替えます（全ノード停止 → 設定変更 → 全ノード起動）。
起動ログに `cluster listener up (mutual TLS)` と出れば有効です。平文のときは警告
`cluster listener up WITHOUT TLS` が出ます。

## 証明書の更新

新しい証明書を同じ CA で作り、ファイルを置き換えてノードを再起動します。CA が同じなら 1 台ずつ再起動して構いません。
