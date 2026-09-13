<?php

namespace WindowsForum\TuiLink;

/**
 * Plain constants (deliberately not an XFCP extension - same rule as
 * NodeRoutePrefix\Config; extension classes fatal when autoloaded outside
 * their chain).
 */
final class Config
{
	/**
	 * The OAuth client this add-on bootstraps is normally named by the
	 * `wfTuiLinkClientId` option. When the option is empty, the client row
	 * with this title is used instead - the arrangement the original
	 * WindowsForum install shipped with, so upgrading needs no new setting.
	 */
	public const CLIENT_TITLE = 'WindowsForum TUI';

	/** Login bootstrap records live this long (seconds). */
	public const TTL = 600;

	/** Input validation bounds for the client-supplied PKCE fields. */
	public const STATE_MIN = 8;
	public const STATE_MAX = 64;
	public const CHALLENGE_LEN = 43;

	public static function boardUrl(): string
	{
		return rtrim(\XF::options()->boardUrl, '/');
	}

	public static function doneUrl(): string
	{
		return self::boardUrl() . '/tui-done';
	}

	/**
	 * The OAuth client the terminal client signs in through, or null when
	 * none is configured / the configured one is inactive.
	 */
	public static function client(): ?\XF\Entity\OAuthClient
	{
		$clientId = trim((string) (\XF::options()->wfTuiLinkClientId ?? ''));
		$client = $clientId !== ''
			? \XF::em()->find('XF:OAuthClient', $clientId)
			: \XF::em()->findOne('XF:OAuthClient', ['title' => self::CLIENT_TITLE]);
		if (!$client || !$client->active)
		{
			return null;
		}
		return $client;
	}
}
