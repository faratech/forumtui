<?php

namespace WindowsForum\TuiLink\Pub\Controller;

use WindowsForum\TuiLink\Service\LinkStore;

/**
 * GET /tui-done?code=..&state=.. - registered OAuth redirect URI. Captures
 * the authorization response, attaches the code to the bootstrap record and
 * tells the human to go back to the terminal (the client picks the code up
 * by polling).
 */
class DoneController extends \XF\Pub\Controller\AbstractController
{
	public function actionIndex(\XF\Mvc\ParameterBag $params)
	{
		$code = $this->filter('code', 'str');
		$state = $this->filter('state', 'str');

		$id = LinkStore::getIdByState($state);

		// An authorization denial arrives as ?error= instead of a code. The
		// record learns it too, so the terminal's next poll says `denied`
		// instead of waiting out the link.
		if (!$code)
		{
			if ($id)
			{
				LinkStore::markDenied($id);
			}
			return $this->message(\XF::phrase('wf_tuilink.denied'));
		}

		if (!$id || !LinkStore::attachCode($id, $code))
		{
			// A revisit of this tab after the terminal already collected its
			// code: that sign-in worked, and the code is not re-armed
			// (forumtui#134).
			if ($id && LinkStore::isConsumed($id))
			{
				return $this->message(\XF::phrase('wf_tuilink.authorized'));
			}
			return $this->message(\XF::phrase('wf_tuilink.expired'));
		}

		return $this->message(\XF::phrase('wf_tuilink.authorized'));
	}
}
