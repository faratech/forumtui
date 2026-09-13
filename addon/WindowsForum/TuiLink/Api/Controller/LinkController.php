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

		$id = LinkStore::create($state, $challenge);
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
