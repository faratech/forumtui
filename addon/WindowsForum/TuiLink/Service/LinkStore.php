<?php

namespace WindowsForum\TuiLink\Service;

use WindowsForum\TuiLink\Config;

/**
 * Store for one-time login bootstrap records. A record holds only the
 * PUBLIC half of PKCE (the S256 challenge) plus the client's state value -
 * the verifier never leaves the TUI, so a leaked record cannot exchange a
 * code.
 *
 * Backed by its own table rather than the global cache: `\XF::app()->cache()`
 * is null on a forum whose config.php enables no cache provider (the
 * XenForo default), and a login relay that fatals on most installs is not
 * a relay. Expired rows are swept on every create.
 */
final class LinkStore
{
	private const TABLE = 'xf_wf_tuilink_link';

	private static function db(): \XF\Db\AbstractAdapter
	{
		return \XF::db();
	}

	/** Above this many live (unexpired) records the endpoint refuses: the
	 * register action is world-callable and this is the only cap on table
	 * growth within the TTL (issue #63). Generous against any real traffic —
	 * one login needs one record — but far below table-flood levels.
	 */
	private const LIVE_CAP = 1000;

	/**
	 * @return string The new link id.
	 * @throws \WindowsForum\TuiLink\Service\LinkStoreException On a full
	 *         table (code "full") or a duplicate state (code "duplicate").
	 */
	public static function create(string $state, string $challenge): string
	{
		$db = self::db();
		$db->delete(self::TABLE, 'expiry_date < ?', \XF::$time);
		$live = $db->fetchOne('SELECT COUNT(*) FROM ' . self::TABLE . ' WHERE expiry_date >= ?', [\XF::$time]);
		if ($live >= self::LIVE_CAP)
		{
			throw new LinkStoreException('full');
		}
		$id = \XF::generateRandomString(12);
		try
		{
			$db->insert(self::TABLE, [
				'link_id' => $id,
				'state' => $state,
				'state_hash' => hash('sha256', $state),
				'challenge' => $challenge,
				'code' => null,
				'denied' => 0,
				'expiry_date' => \XF::$time + Config::TTL,
			]);
		}
		catch (\XF\Db\DuplicateKeyException $e)
		{
			// A UNIQUE state_hash re-registered inside its TTL is a client
			// bug or a retry, not a server fault: answer cleanly instead of
			// surfacing an XF error page (issue #63).
			throw new LinkStoreException('duplicate');
		}
		return $id;
	}

	/**
	 * @return array{state: string, challenge: string, code: ?string, denied: int}|null
	 */
	public static function get(string $id): ?array
	{
		$row = self::db()->fetchRow(
			'SELECT state, challenge, code, denied FROM ' . self::TABLE
			. ' WHERE link_id = ? AND expiry_date >= ?',
			[$id, \XF::$time]
		);
		return $row ?: null;
	}

	public static function getIdByState(string $state): ?string
	{
		$id = self::db()->fetchOne(
			'SELECT link_id FROM ' . self::TABLE . ' WHERE state_hash = ? AND expiry_date >= ?',
			[hash('sha256', $state), \XF::$time]
		);
		return is_string($id) && $id !== '' ? $id : null;
	}

	public static function attachCode(string $id, string $code): bool
	{
		return self::db()->update(
			self::TABLE,
			['code' => $code],
			'link_id = ? AND expiry_date >= ?',
			[$id, \XF::$time]
		) > 0;
	}

	/**
	 * Burns the authorization code on first successful poll (#766a). The
	 * compare-and-clear means exactly one poll ever observes the code, even
	 * with concurrent pollers racing the legitimate client. The row itself
	 * stays alive until TTL so status/denied keep answering; a repeat poll
	 * after consumption simply finds no code.
	 */
	public static function consumeCode(string $id, string $code): bool
	{
		return self::db()->update(
			self::TABLE,
			['code' => null],
			'link_id = ? AND code = ? AND expiry_date >= ?',
			[$id, $code, \XF::$time]
		) > 0;
	}

	/** Per-IP register budget (#766b). */
	public const REGISTER_LIMIT = 10;
	/** Window in seconds for the register budget. */
	public const REGISTER_WINDOW = 3600;

	/**
	 * Reserves one /register slot for this IP (~10/hour, #766b). Best-effort:
	 * with no Redis backend configured, or on a Redis error, the slot is
	 * granted — this endpoint only mints a public relay row and PKCE makes
	 * a flooded record useless, so availability of the login path wins over
	 * the counter. LIVE_CAP still bounds the table either way. SharedRedis
	 * exists only on windowsforum.com; other forums skip the counter.
	 */
	public static function reserveRegisterSlot(string $ip): bool
	{
		if (!class_exists(\WindowsForum\SharedRedis::class))
		{
			return true;
		}
		$redis = \WindowsForum\SharedRedis::raw();
		if (!$redis)
		{
			return true;
		}
		$key = 'wf_tuilink:reg:' . hash_hmac('sha256', $ip, (string) \XF::config('globalSalt'));
		try
		{
			$count = (int) $redis->incr($key);
			if ($count === 1)
			{
				$redis->expire($key, self::REGISTER_WINDOW);
			}
			return $count <= self::REGISTER_LIMIT;
		}
		catch (\Throwable $e)
		{
			return true;
		}
	}

	/** The person declined the authorization request. */
	public static function markDenied(string $id): bool
	{
		return self::db()->update(
			self::TABLE,
			['denied' => 1],
			'link_id = ? AND expiry_date >= ?',
			[$id, \XF::$time]
		) > 0;
	}
}
