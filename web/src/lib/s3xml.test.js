import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  attachment,
  bucketNameError,
  corsXml,
  deleteObjectsXml,
  encodeKey,
  lifecycleXml,
  objectLockXml,
  partSize,
  queryString,
} from './s3xml.js';
import { basename, bytes, duration, parseBytes } from './format.js';

test('keys are encoded per segment', () => {
  assert.equal(encodeKey("a b/c+d!'()*/日"), 'a%20b/c%2Bd%21%27%28%29%2A/%E6%97%A5');
});

test('query strings keep bare subresources', () => {
  assert.equal(queryString({ uploads: true, prefix: 'a b', x: undefined }), '?uploads&prefix=a%20b');
  assert.equal(queryString({}), '');
});

test('xml bodies escape their values', () => {
  assert.equal(
    deleteObjectsXml([{ key: 'a<&>' }, { key: 'b', versionId: 'v1' }]),
    '<Delete><Quiet>true</Quiet><Object><Key>a&lt;&amp;&gt;</Key></Object><Object><Key>b</Key><VersionId>v1</VersionId></Object></Delete>',
  );
});

test('lifecycle rules serialize with a single or combined filter', () => {
  const x = lifecycleXml([
    { id: 'logs', enabled: true, prefix: 'logs/', expirationDays: '30', abortDays: 7 },
    {
      id: 'big',
      enabled: false,
      prefix: 'p/',
      sizeGt: 1024,
      tags: [{ key: 'k', value: 'v' }],
      transitions: [{ days: 10, storageClass: 'STANDARD_IA' }],
      noncurrentDays: 5,
      newerNoncurrentVersions: 2,
    },
    { id: 'markers', enabled: true, expiredObjectDeleteMarker: true },
  ]);
  assert.match(x, /<Rule><ID>logs<\/ID><Filter><Prefix>logs\/<\/Prefix><\/Filter><Status>Enabled<\/Status><Expiration><Days>30<\/Days><\/Expiration><AbortIncompleteMultipartUpload><DaysAfterInitiation>7<\/DaysAfterInitiation>/);
  assert.match(x, /<Filter><And><Prefix>p\/<\/Prefix><Tag><Key>k<\/Key><Value>v<\/Value><\/Tag><ObjectSizeGreaterThan>1024<\/ObjectSizeGreaterThan><\/And><\/Filter><Status>Disabled/);
  assert.match(x, /<Transition><Days>10<\/Days><StorageClass>STANDARD_IA<\/StorageClass><\/Transition>/);
  assert.match(x, /<NoncurrentVersionExpiration><NoncurrentDays>5<\/NoncurrentDays><NewerNoncurrentVersions>2</);
  assert.match(x, /<Filter><\/Filter><Status>Enabled<\/Status><Expiration><ExpiredObjectDeleteMarker>true/);
});

test('cors and object lock bodies', () => {
  assert.equal(
    corsXml([{ origins: ['https://a'], methods: ['GET', 'PUT'], headers: ['*'], maxAge: 600 }]),
    '<CORSConfiguration><CORSRule><AllowedOrigin>https://a</AllowedOrigin><AllowedMethod>GET</AllowedMethod><AllowedMethod>PUT</AllowedMethod><AllowedHeader>*</AllowedHeader><MaxAgeSeconds>600</MaxAgeSeconds></CORSRule></CORSConfiguration>',
  );
  assert.match(objectLockXml({ mode: 'GOVERNANCE', days: 30 }), /<DefaultRetention><Mode>GOVERNANCE<\/Mode><Days>30<\/Days>/);
  assert.doesNotMatch(objectLockXml({ mode: null }), /Rule/);
});

test('helpers', () => {
  assert.equal(attachment('資料 "1".pdf'), `attachment; filename="__ _1_.pdf"; filename*=UTF-8''%E8%B3%87%E6%96%99%20%221%22.pdf`);
  assert.equal(partSize(1), 8 * 1024 * 1024);
  assert.ok(partSize(5 * 1024 ** 4) * 10000 >= 5 * 1024 ** 4);
  assert.equal(bucketNameError('ok-name'), null);
  assert.ok(bucketNameError('Bad'));
  assert.ok(bucketNameError('a..b'));
  assert.equal(bytes(1536), '1.50 KiB');
  assert.equal(parseBytes('10 GiB'), 10 * 1024 ** 3);
  assert.equal(parseBytes(''), null);
  assert.ok(Number.isNaN(parseBytes('ten')));
  assert.equal(basename('a/b/c.txt'), 'c.txt');
  assert.equal(basename('a/b/'), 'b/');
});

test('duration', () => {
  assert.equal(duration(400), '0.4 秒');
  assert.equal(duration(12_400), '12 秒');
  assert.equal(duration(185_000), '3 分 5 秒');
  assert.equal(duration(7_800_000), '2 時間 10 分');
  assert.equal(duration(null), '—');
});
