<?php

// Example client for the suhms admin API, for the SoraHost Laravel portal.
// Copy to app/Services/SuhmsAdminClient.php and set SUHMS_ADMIN_URL / SUHMS_ADMIN_TOKEN in .env
// (exposed via config/services.php as services.suhms.url / services.suhms.token).
// See docs/sorahost-integration.md for the API.

namespace App\Services;

use Illuminate\Http\Client\PendingRequest;
use Illuminate\Http\Client\RequestException;
use Illuminate\Support\Facades\Http;

class SuhmsAdminClient
{
    private function http(): PendingRequest
    {
        return Http::baseUrl(config('services.suhms.url'))
            ->withToken(config('services.suhms.token'))
            ->acceptJson()
            ->timeout(30)
            ->retry(3, 500, fn ($e) => $e instanceof RequestException && $e->response->serverError());
    }

    /** @return array{id: string, name: string, suspended: bool, quota_bytes: ?int, max_buckets: ?int} */
    public function createTenant(string $name, ?int $quotaBytes = null, ?int $maxBuckets = null): array
    {
        return $this->http()->post('/admin/v1/tenants', array_filter([
            'name' => $name,
            'quota_bytes' => $quotaBytes,
            'max_buckets' => $maxBuckets,
        ], fn ($v) => $v !== null))->throw()->json();
    }

    /** Includes current usage: objects, logical_bytes, stored_bytes. */
    public function tenant(string $tenantId): array
    {
        return $this->http()->get("/admin/v1/tenants/{$tenantId}")->throw()->json();
    }

    /** Pass null to remove a limit; omitted arguments stay unchanged. */
    public function updateTenant(string $tenantId, array $changes): array
    {
        return $this->http()->patch("/admin/v1/tenants/{$tenantId}", $changes)->throw()->json();
    }

    public function suspend(string $tenantId, bool $suspended = true): array
    {
        return $this->updateTenant($tenantId, ['suspended' => $suspended]);
    }

    /**
     * The secret is returned only here: show it to the customer once, do not store it in plain text.
     *
     * @return array{access_key_id: string, secret_access_key: string}
     */
    public function issueKey(string $tenantId): array
    {
        return $this->http()->post("/admin/v1/tenants/{$tenantId}/keys")->throw()->json();
    }

    public function keys(string $tenantId): array
    {
        return $this->http()->get("/admin/v1/tenants/{$tenantId}/keys")->throw()->json();
    }

    public function setKeyActive(string $accessKeyId, bool $active): void
    {
        $this->http()->patch("/admin/v1/keys/{$accessKeyId}", ['active' => $active])->throw();
    }

    public function buckets(string $tenantId): array
    {
        return $this->http()->get("/admin/v1/tenants/{$tenantId}/buckets")->throw()->json();
    }

    /** $replicas: 1, 2, 3 or null (cluster default). */
    public function setBucketReplicas(string $bucket, ?int $replicas): array
    {
        return $this->http()->patch("/admin/v1/buckets/{$bucket}", ['replicas' => $replicas])->throw()->json();
    }

    public function setBucketPublic(string $bucket, bool $public): array
    {
        return $this->http()->patch("/admin/v1/buckets/{$bucket}", ['public_read' => $public])->throw()->json();
    }

    /**
     * Usage between two instants (unix ms), for monthly billing.
     * GB-months = logical_byte_hours / 1e9 / hours in the month.
     */
    public function usage(string $tenantId, \DateTimeInterface $from, \DateTimeInterface $to): array
    {
        return $this->http()->get("/admin/v1/tenants/{$tenantId}/usage", [
            'from' => $from->getTimestamp() * 1000,
            'to' => $to->getTimestamp() * 1000,
        ])->throw()->json();
    }
}
