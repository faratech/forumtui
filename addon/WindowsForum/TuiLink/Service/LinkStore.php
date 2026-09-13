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

	public static function create(string $state, string $challenge): string
	{
		$db = self::db();
		$db->delete(self::TABLE, 'expiry_date < ?', \XF::$time);
		$id = \XF::generateRandomString(12);
		$db->insert(self::TABLE, [
			'link_id' => $id,
			'state' => $state,
			'state_hash' => hash('sha256', $state),
			'challenge' => $challenge,
			'code' => null,
			'denied' => 0,
			'expiry_date' => \XF::$time + Config::TTL,
		]);
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
