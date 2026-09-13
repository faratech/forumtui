<?php

namespace WindowsForum\TuiLink\Pub\Controller;

use WindowsForum\TuiLink\Config;
use WindowsForum\TuiLink\Service\LinkStore;

/**
 * GET /tui-start/<id> - 302 into the OAuth authorize page with the PKCE
 * challenge + state stored under <id>. The short URL is what the terminal
 * client displays, so it fits any screen and can be typed by hand.
 */
class StartController extends \XF\Pub\Controller\AbstractController
{
	public function actionIndex(\XF\Mvc\ParameterBag $params)
	{
		$record = LinkStore::get((string) $params->id);
		if (!$record || empty($record['challenge']) || empty($record['state']))
		{
			return $this->message(\XF::phrase('wf_tuilink.expired'));
		}

		$client = Config::client();
		if (!$client)
		{
			return $this->message(\XF::phrase('wf_tuilink.unavailable'));
		}

		$query = http_build_query([
			'response_type' => 'code',
			'client_id' => $client->client_id,
			'redirect_uri' => Config::doneUrl(),
			'scope' => implode(' ', $client->allowed_scopes),
			'state' => $record['state'],
			'code_challenge' => $record['challenge'],
			'code_challenge_method' => 'S256',
		], '', '&', PHP_QUERY_RFC3986);

		return $this->redirect(Config::boardUrl() . '/oauth2/authorize?' . $query);
	}
}
