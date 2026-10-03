<?php

namespace WindowsForum\TuiLink\Api\Controller;

use WindowsForum\TuiLink\Config;
use WindowsForum\TuiLink\Service\LinkStore;

/**
 * Unauthenticated API surface for the TUI login bootstrap:
 *
 *   POST /api/wf-tuilink/register  {state, challenge} -> {id, url}
 *   POST /api/wf-tuilink/poll      {id}               -> {status: waiting|authorized|denied|expired, code?}
 *
 * No credentials flow through here, and PKCE (S256) makes any intercepted
 * code unexercisable without the client's verifier.
 */
class LinkController extends \XF\Api\Controller\AbstractController
{
	/**
	 * Only the two login-bootstrap actions are public. This used to be a
	 * blanket `return true`, which meant any action added to this class later
	 * would be world-readable by accident — and a sibling controller for
	 * authenticated work (drafts, wftui #716) would have inherited it. Naming
	 * them makes the public surface a decision rather than a default.
	 */
	public function allowUnauthenticatedRequest($action)
	{
		// The dispatcher hands this the HTTP verb glued to the URL segment and
		// lowercased — "postregister", not "register" (XF\Mvc\Router sets the
		// raw suffix; XF\Api\App::preDispatch passes it through unnormalised).
		// An allowlist of the bare segments would have refused every login.
		return (bool) preg_match(
			'/^(?:get|post|put|delete|patch)?(?:register|poll)$/',
			strtolower($action)
		);
	}

	public function actionPostRegister()
	{
		// Per-IP budget before any row is minted (#766b); the generic 429 does
		// not confirm whether the limit or the request shape failed.
		if (!LinkStore::reserveRegisterSlot($this->request()->getIp()))
		{
			return $this->apiError(
				\XF::phrase('oops_we_ran_into_some_problems'),
				'wf_tuilink_rate_limited',
				null,
				429
			);
		}

		$state = $this->filter('state', 'str');
		$challenge = $this->filter('challenge', 'str');

		if (!preg_match('/^[A-Za-z0-9_\-]{' . Config::STATE_MIN . ',' . Config::STATE_MAX . '}$/', $state))
		{
			return $this->apiError(\XF::phrase('wf_tuilink.invalid_state'), 'wf_tuilink_invalid_state', null, 400);
		}
		if (!preg_match('/^[A-Za-z0-9_\-]{' . Config::CHALLENGE_LEN . '}$/', $challenge))
		{
			return $this->apiError(\XF::phrase('wf_tuilink.invalid_challenge'), 'wf_tuilink_invalid_challenge', null, 400);
		}

		try
		{
			$id = LinkStore::create($state, $challenge);
		}
		catch (\WindowsForum\TuiLink\Service\LinkStoreException $e)
		{
			if ($e->codeName() === 'duplicate')
			{
				return $this->apiError(\XF::phrase('wf_tuilink.duplicate_state'), 'wf_tuilink_duplicate_state', null, 409);
			}
			return $this->apiError(\XF::phrase('wf_tuilink.busy'), 'wf_tuilink_busy', null, 503);
		}
		return $this->apiResult([
			'success' => true,
			'id' => $id,
			'url' => Config::boardUrl() . '/tui-start/' . $id,
			'ttl' => Config::TTL,
		]);
	}

	public function actionPostPoll()
	{
		$id = $this->filter('id', 'str');
		if (!preg_match('/^[A-Za-z0-9_\-]{6,32}$/', $id))
		{
			return $this->apiError(\XF::phrase('wf_tuilink.invalid_id'), 'wf_tuilink_invalid_id', null, 400);
		}

		$record = LinkStore::get($id);
		if (!$record)
		{
			return $this->apiResult(['success' => true, 'status' => 'expired']);
		}
		if (!empty($record['code']))
		{
			// Burn on first read (#766a): the compare-and-clear in LinkStore
			// guarantees exactly one poll ever carries the code. The wftui
			// client polls until the code appears and stops; if this response
			// is lost the repeat poll lands in the codeless "waiting" shape
			// below and the client simply polls until TTL expiry.
			if (!LinkStore::consumeCode($id, $record['code']))
			{
				return $this->apiResult(['success' => true, 'status' => 'waiting']);
			}
			return $this->apiResult([
				'success' => true,
				'status' => 'authorized',
				'code' => $record['code'],
			]);
		}
		if (!empty($record['denied']))
		{
			return $this->apiResult(['success' => true, 'status' => 'denied']);
		}
		return $this->apiResult(['success' => true, 'status' => 'waiting']);
	}
}
